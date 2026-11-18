//! The trade engine: one [`TradeModel`] per trade of one trader.
//!
//! Every request of the caller is one operation of the trade model. An operation first checks its
//! preconditions, then validates all data, and only then changes the trade. The rules are
//! specified in `docs/specifications/trade/trade-states.md`.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt::Display;
use std::sync::{Arc, Mutex};

use bdk_wallet::bitcoin::address::{NetworkChecked, NetworkUnchecked, NetworkValidation};
use bdk_wallet::bitcoin::hashes::Hash as _;
use bdk_wallet::bitcoin::key::{Keypair, Secp256k1};
use bdk_wallet::bitcoin::secp256k1::{Message, SecretKey};
use bdk_wallet::bitcoin::{
    Address, Amount, FeeRate, Network, OutPoint, Psbt, ScriptBuf, TapSighash, TapSighashType,
    Transaction, Txid, XOnlyPublicKey, taproot,
};
use musig2::secp::{MaybeScalar, Point, Scalar};
use musig2::{PartialSignature, PubNonce};
use thiserror::Error;
use tracing::{error, info, instrument};
use wallet::protocol_wallet_api::ProtocolWalletApi;

use crate::chain_facts::{ChainFacts, Spend, TradeEvent, TxKind, TxToSend};
use crate::multisig::{KeyCtx, KeyPair, PointExt as _, SigCtx};
use crate::params::{EngineConfig, MIN_CONFIRMATIONS, REDIRECT_LOCK_TIME, TradeParams};
use crate::receiver::{Receiver, ReceiverList};
use crate::script_paths;
use crate::state::{ClosureType, TradeState};
use crate::storage::{ByRef, ByVal, Storage};
use crate::transaction::{
    CustomPayoutTxBuilder, DepositTxBuilder, PenaltyTxBuilder, RedirectTxBuilder, SwapTxBuilder,
    TransactionExt as _, WarningTxBuilder, build_claim_tx,
};

/// Holds the trades of one trader.
pub trait TradeModelStore: Send + Sync {
    /// Whether a trade with this ID is stored. A trade ID identifies one trade for the whole
    /// lifetime of the store, also if that trade is closed.
    fn contains_trade(&self, trade_id: &str) -> bool;

    /// Stores a new trade model. A trade model with the ID of a stored trade is rejected, and the
    /// stored trade model is not changed.
    fn add_trade_model(&self, trade_model: TradeModel) -> Result<()>;

    fn get_trade_model(&self, trade_id: &str) -> Option<Arc<Mutex<TradeModel>>>;

    /// All trades of the store.
    fn trade_models(&self) -> Vec<Arc<Mutex<TradeModel>>>;

    /// The ID of the trade that has reserved the coin `coin` of the wallet for its deposit, if
    /// there is one. A coin of such a trade must be released through the trade
    /// ([`TradeModel::abandon`]), never directly in the wallet.
    fn trade_reserving(&self, coin: &OutPoint) -> Option<String>;
}

/// A trade store that keeps the trades in memory. They are lost when the process ends.
#[derive(Default)]
pub struct MemoryTradeStore(Mutex<BTreeMap<String, Arc<Mutex<TradeModel>>>>);

impl TradeModelStore for MemoryTradeStore {
    fn contains_trade(&self, trade_id: &str) -> bool {
        self.0.lock().unwrap().contains_key(trade_id)
    }

    fn trade_reserving(&self, coin: &OutPoint) -> Option<String> {
        // The trades are copied first, so that the lock of the store is not held while the lock
        // of a trade is taken.
        let trades = self.0.lock().unwrap().values().cloned().collect::<Vec<_>>();
        trades.into_iter().find_map(|trade| {
            let trade = trade.lock().unwrap();
            trade
                .reserved_coins
                .contains(coin)
                .then(|| trade.trade_id.clone())
        })
    }

    fn add_trade_model(&self, trade_model: TradeModel) -> Result<()> {
        match self.0.lock().unwrap().entry(trade_model.trade_id.clone()) {
            Entry::Occupied(entry) => Err(ProtocolErrorKind::DuplicateTradeId(entry.key().clone())),
            Entry::Vacant(entry) => {
                entry.insert(Arc::new(Mutex::new(trade_model)));
                Ok(())
            }
        }
    }

    fn get_trade_model(&self, trade_id: &str) -> Option<Arc<Mutex<TradeModel>>> {
        self.0.lock().unwrap().get(trade_id).map(Arc::clone)
    }

    fn trade_models(&self) -> Vec<Arc<Mutex<TradeModel>>> {
        self.0.lock().unwrap().values().cloned().collect()
    }
}

#[derive(Clone)]
pub struct TradeModel {
    trade_id: String,
    my_role: Role,
    network: Network,
    /// The state that the requests of the caller and the engine's own decisions gave the trade.
    /// [`Self::state`] is this state, unless the chain shows a later one.
    state: TradeState,
    /// The state that follows from the confirmed transactions of the trade, if it differs from
    /// `state`. It is computed again from every observation of the chain, so it goes back when a
    /// reorganisation removes the transaction that caused it.
    observed_state: Option<TradeState>,
    /// The facts about the chain that the engine was told last.
    chain: ChainFacts,
    /// The height of the chain tip at the first observation after the trader signed the deposit
    /// transaction. The deposit timeout is counted from here.
    deposit_watch_start: Option<u32>,
    /// Set, with the reason, when the peer sent data that an unmodified client never produces.
    /// A terminated trade accepts no further data. Its state changes only once more, and only
    /// if it was terminated during the setup: then the trade is abandoned.
    terminated: Option<String>,
    /// The coins of the wallet that are reserved for the trader's half of the deposit
    /// transaction. They are locked in the wallet from the moment the half deposit exists.
    reserved_coins: Vec<OutPoint>,
    /// The height of the chain tip of the wallet when the trader signed the deposit transaction.
    /// The deposit transaction cannot be confirmed at or below this height.
    deposit_signing_height: Option<u32>,
    /// The data of the requests that were processed. A repeated request is compared with it.
    received: Received,
    keys: Keys,
    deposit_tx: DepositTx,
    swap_tx: SwapTx,
    penalty_tx: PenaltyTx,
    custom_payout_tx: CustomPayoutTx,
    buyer_txs: ArbitrationTxs,
    seller_txs: ArbitrationTxs,
}

#[expect(
    clippy::exhaustive_enums,
    reason = "the four trade roles are a closed set"
)]
#[derive(Copy, Clone, Eq, PartialEq, Default, Debug)]
pub enum Role {
    #[default]
    SellerAsMaker,
    SellerAsTaker,
    BuyerAsMaker,
    BuyerAsTaker,
}

#[derive(Clone, Default)]
struct Received {
    params: Option<TradeParams>,
    /// The configuration with which the trade was set up. A trade keeps its lock times when the
    /// configuration of the engine changes later.
    config: Option<EngineConfig>,
    nonce_shares: Option<ReceivedNonceShares>,
    partial_signatures: Option<ReceivedPartialSignatures>,
    payment_started: bool,
    swap_tx_signature: Option<PartialSignature>,
    custom_payout_request: Option<(Amount, FeeRate)>,
}

#[derive(Clone, Eq, PartialEq)]
struct ReceivedNonceShares {
    half_deposit_psbt: Psbt,
    addresses: [Address<NetworkUnchecked>; 3],
    nonce_shares: [PubNonce; 7],
    redirection_receivers: Vec<Receiver<NetworkUnchecked>>,
}

impl ReceivedNonceShares {
    fn new(
        (half_deposit_psbt, addresses, nonce_shares): PeersNonceSharesMessage,
        redirection_receivers: Vec<Receiver<NetworkUnchecked>>,
    ) -> Self {
        Self {
            half_deposit_psbt,
            addresses: [
                addresses.warning_tx_fee_bump,
                addresses.redirect_tx_fee_bump,
                addresses.claim_tx_payout,
            ],
            nonce_shares: [
                nonce_shares.swap_tx_input,
                nonce_shares.buyers_warning_tx_buyer_input,
                nonce_shares.buyers_warning_tx_seller_input,
                nonce_shares.sellers_warning_tx_buyer_input,
                nonce_shares.sellers_warning_tx_seller_input,
                nonce_shares.buyers_redirect_tx_input,
                nonce_shares.sellers_redirect_tx_input,
            ],
            redirection_receivers,
        }
    }
}

/// The data of the peer in the second message of the setup: its half of the deposit transaction,
/// its addresses and its nonce shares.
pub type PeersNonceSharesMessage = (
    Psbt,
    ExchangedAddresses<'static, ByVal, NetworkUnchecked>,
    ExchangedNonces<'static, ByVal>,
);

#[derive(Clone, Eq, PartialEq)]
struct ReceivedPartialSignatures {
    warning_tx_buyer: PartialSignature,
    warning_tx_seller: PartialSignature,
    redirect_tx: PartialSignature,
    /// Only the buyer uses the seller's data for the swap transaction during the setup.
    swap_tx: Option<(PartialSignature, TapSighash)>,
}

#[derive(Clone, Default)]
struct Keys {
    am_buyer: bool,
    buyer_payout_ctx: KeyCtx,
    seller_payout_ctx: KeyCtx,
    my_multisig_script_key: Option<XOnlyPublicKey>,
    peers_multisig_script_key: Option<XOnlyPublicKey>,
}

/// The pre-signed transactions of one trader for the dispute path, and the address of his wallet
/// that receives his output of a custom payout, of his penalty transaction and of his claim
/// transaction.
#[derive(Clone, Default)]
struct ArbitrationTxs {
    warning: WarningTx,
    redirect: RedirectTx,
    payout_address: Option<Address>,
}

#[derive(Clone, Default)]
struct DepositTx {
    builder: DepositTxBuilder,
}

#[derive(Clone, Default)]
struct SwapTx {
    builder: SwapTxBuilder,
    input_sighash: Option<TapSighash>,
    input_sig_ctx: SigCtx,
}

#[derive(Clone, Default)]
struct WarningTx {
    builder: WarningTxBuilder,
    buyer_input_sig_ctx: SigCtx,
    seller_input_sig_ctx: SigCtx,
}

#[derive(Clone, Default)]
struct RedirectTx {
    builder: RedirectTxBuilder,
    input_sig_ctx: SigCtx,
}

#[derive(Clone, Default)]
struct PenaltyTx {
    builder: PenaltyTxBuilder,
    input_sig_ctx: SigCtx,
}

#[derive(Clone, Default)]
struct CustomPayoutTx {
    builder: CustomPayoutTxBuilder,
}

pub struct ExchangedKeys<'a, S: Storage> {
    pub buyer_payout: S::Store<'a, Point>,
    pub seller_payout: S::Store<'a, Point>,
    pub multisig_script: S::Store<'a, XOnlyPublicKey>,
}

pub struct ExchangedAddresses<'a, S: Storage, V: NetworkValidation + 'a = NetworkChecked> {
    pub warning_tx_fee_bump: S::Store<'a, Address<V>>,
    pub redirect_tx_fee_bump: S::Store<'a, Address<V>>,
    pub claim_tx_payout: S::Store<'a, Address<V>>,
}

pub struct ExchangedNonces<'a, S: Storage> {
    pub swap_tx_input: S::Store<'a, PubNonce>,
    pub buyers_warning_tx_buyer_input: S::Store<'a, PubNonce>,
    pub buyers_warning_tx_seller_input: S::Store<'a, PubNonce>,
    pub sellers_warning_tx_buyer_input: S::Store<'a, PubNonce>,
    pub sellers_warning_tx_seller_input: S::Store<'a, PubNonce>,
    pub buyers_redirect_tx_input: S::Store<'a, PubNonce>,
    pub sellers_redirect_tx_input: S::Store<'a, PubNonce>,
}

pub struct ExchangedSigs<'a, S: Storage> {
    pub peers_warning_tx_buyer_input_partial_signature: S::Store<'a, PartialSignature>,
    pub peers_warning_tx_seller_input_partial_signature: S::Store<'a, PartialSignature>,
    pub peers_redirect_tx_input_partial_signature: S::Store<'a, PartialSignature>,
    pub swap_tx_input_partial_signature: Option<S::Store<'a, PartialSignature>>,
    pub swap_tx_input_sighash: Option<S::Store<'a, TapSighash>>,
    pub contractual_txids: Option<ContractualTxids>,
}

pub struct ContractualTxids {
    pub deposit: Txid,
    pub buyers_warning: Txid,
    pub sellers_warning: Txid,
    pub buyers_redirect: Txid,
    pub sellers_redirect: Txid,
}

/// Why an operation did not complete.
enum Failure {
    /// The request is refused. The trade is not changed, and the request can be repeated.
    Refused(ProtocolErrorKind),
    /// The peer sent data that an unmodified client never produces. The trade is terminated with
    /// this reason.
    PeerMisbehaved(String),
}

impl<E: Into<ProtocolErrorKind>> From<E> for Failure {
    fn from(error: E) -> Self {
        Self::Refused(error.into())
    }
}

fn peer_misbehaved(reason: impl Into<String>) -> Failure {
    Failure::PeerMisbehaved(reason.into())
}

/// Data of a message of the peer, as the caller of the engine decoded it. `Err` holds the reason
/// why a field cannot be decoded.
///
/// An operation takes the result of the decoding, not the decoded value, so that the engine keeps
/// its order of checks (D18): it checks the preconditions of the request first, and only then
/// handles data that cannot be decoded as a failed check of the peer's data (D17).
pub type PeerData<T> = Result<T, UndecodablePeerData>;

/// The reason why a field of a message of the peer cannot be decoded. It appears in the log and
/// in the error for the caller, so it must not contain a secret.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UndecodablePeerData(pub String);

/// Returns the decoded data of the peer. Data that cannot be decoded is a failed check of the
/// peer's data: an unmodified client never sends it.
fn decoded<T>(data: PeerData<T>) -> Result<T, Failure> {
    data.map_err(|UndecodablePeerData(reason)| {
        peer_misbehaved(format!(
            "a field of a message of the peer cannot be decoded: {reason}"
        ))
    })
}

/// Marks the checks of data that the peer created. An unmodified client passes them.
trait PeerDataCheck<T> {
    fn or_peer_misbehaved(self, what: &str) -> Result<T, Failure>;
}

impl<T, E: Display> PeerDataCheck<T> for Result<T, E> {
    fn or_peer_misbehaved(self, what: &str) -> Result<T, Failure> {
        self.map_err(|error| peer_misbehaved(format!("{what}: {error}")))
    }
}

macro_rules! access {
    ($name:ident, $pattern:pat $(if $guard:expr)?) => {
        {
            use ClosureType::*;
            use TradeState::*;
            #[expect(path_statements, reason="HACK to prevent `unused_imports` warning")]
            Cooperative;
            match $name.state() {
                $pattern $(if $guard)? => Ok(()),
                _ => Err(ProtocolErrorKind::AccessDenied)
            }
        }
    };
}

// ---- Creation and general properties ----

impl TradeModel {
    /// Creates the trade of one trader. The wallet supplies the network, the key shares and the
    /// script key.
    pub fn new(
        trade_id: String,
        my_role: Role,
        wallet: &mut dyn ProtocolWalletApi,
    ) -> Result<Self> {
        let am_buyer = matches!(my_role, Role::BuyerAsMaker | Role::BuyerAsTaker);
        let mut keys = Keys {
            am_buyer,
            ..Keys::default()
        };
        // The key shares are derived from the seed of the wallet and the trade ID (D10). The
        // engine keeps them in memory only.
        let key_shares = wallet.trade_key_shares(&trade_id)?;
        keys.buyer_payout_ctx
            .set_my_key_share(key_shares.buyer_payout);
        keys.seller_payout_ctx
            .set_my_key_share(key_shares.seller_payout);
        keys.my_multisig_script_key = Some(wallet.new_internal_key()?);

        let mut trade_model = Self {
            trade_id,
            my_role,
            network: wallet.network(),
            state: TradeState::Init,
            observed_state: None,
            chain: ChainFacts::default(),
            deposit_watch_start: None,
            terminated: None,
            reserved_coins: Vec::new(),
            deposit_signing_height: None,
            received: Received::default(),
            keys,
            deposit_tx: DepositTx::default(),
            swap_tx: SwapTx::default(),
            penalty_tx: PenaltyTx::default(),
            custom_payout_tx: CustomPayoutTx::default(),
            buyer_txs: ArbitrationTxs::default(),
            seller_txs: ArbitrationTxs::default(),
        };
        for txs in [&mut trade_model.buyer_txs, &mut trade_model.seller_txs] {
            txs.redirect.builder.set_lock_time(REDIRECT_LOCK_TIME);
        }
        trade_model.swap_tx.builder.disable_lock_time();
        trade_model.penalty_tx.builder.disable_lock_time();
        Ok(trade_model)
    }

    pub fn trade_id(&self) -> &str {
        &self.trade_id
    }

    pub const fn role(&self) -> Role {
        self.my_role
    }

    pub const fn am_buyer(&self) -> bool {
        self.keys.am_buyer
    }

    /// The state of the trade: the state that the requests gave it, or the later state that
    /// the confirmed transactions of the trade show.
    pub fn state(&self) -> TradeState {
        self.observed_state.unwrap_or(self.state)
    }

    /// The reason why the trade was terminated, if it was.
    pub fn termination_reason(&self) -> Option<&str> {
        self.terminated.as_deref()
    }

    fn check_not_terminated(&self) -> Result<()> {
        match &self.terminated {
            Some(reason) => Err(ProtocolErrorKind::TradeTerminated(reason.clone())),
            None => Ok(()),
        }
    }

    /// Terminates the trade, because the peer sent data that an unmodified client never
    /// produces. The reason is logged with its details, so that the user can see why the trade
    /// ended. It must not contain a secret.
    fn terminate(&mut self, reason: String) -> ProtocolErrorKind {
        error!(
            trade_id = self.trade_id,
            role = ?self.my_role,
            state = %self.state,
            reason,
            "Terminating the trade: the peer does not follow the protocol."
        );
        self.terminated = Some(reason.clone());
        ProtocolErrorKind::TradeTerminated(reason)
    }

    /// Runs one operation on a copy of the trade. The copy replaces the trade only if the
    /// operation succeeds, so a request that is refused leaves no data and no signature. If the
    /// operation finds that the peer misbehaved, the trade is terminated.
    fn atomically<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<T, Failure>,
    ) -> Result<T> {
        self.check_not_terminated()?;
        let mut draft = self.clone();
        match operation(&mut draft) {
            Ok(value) => {
                *self = draft;
                Ok(value)
            }
            Err(Failure::Refused(error)) => Err(error),
            Err(Failure::PeerMisbehaved(reason)) => Err(self.terminate(reason)),
        }
    }

    const fn require_buyer(&self) -> Result<()> {
        if self.am_buyer() {
            Ok(())
        } else {
            Err(ProtocolErrorKind::WrongRole("buyer"))
        }
    }

    const fn require_seller(&self) -> Result<()> {
        if self.am_buyer() {
            Err(ProtocolErrorKind::WrongRole("seller"))
        } else {
            Ok(())
        }
    }

    fn require_state(&self, allowed: &[TradeState]) -> Result<()> {
        let state = self.state();
        if allowed.contains(&state) {
            Ok(())
        } else {
            Err(ProtocolErrorKind::NotAllowedInState(state))
        }
    }

    /// The deposit transaction counts as confirmed at [`MIN_CONFIRMATIONS`] confirmations. Before
    /// that, no signature for the swap transaction, no key share and no signature for a custom
    /// payout leaves the engine (D6, F9).
    fn require_confirmed_deposit(&self) -> Result<()> {
        let confirmations = self.chain.deposit_confirmations;
        if confirmations >= u32::from(MIN_CONFIRMATIONS) {
            Ok(())
        } else {
            Err(ProtocolErrorKind::ChainCondition(format!(
                "the deposit transaction has {confirmations} of {MIN_CONFIRMATIONS} confirmations"
            )))
        }
    }

    fn config(&self) -> Result<EngineConfig> {
        self.received
            .config
            .ok_or(ProtocolErrorKind::OutOfOrder("the trade is not set up"))
    }

    /// The number of blocks that remain before the buyer's warning transaction becomes valid:
    /// it is valid when the deposit transaction has as many confirmations as the warning lock
    /// time.
    fn blocks_before_warning_is_valid(&self) -> Result<u32> {
        let warning_lock_time = u32::from(self.config()?.warning_lock_time_blocks());
        Ok(warning_lock_time.saturating_sub(self.chain.deposit_confirmations))
    }

    #[instrument(skip_all)]
    fn update_state(&mut self, new_state: TradeState) -> Result<bool> {
        self.check_not_terminated()?;
        self.set_state(new_state)
    }

    /// Changes the state, also of a terminated trade.
    fn set_state(&mut self, new_state: TradeState) -> Result<bool> {
        if self.state >= new_state {
            return Ok(false);
        }
        let (role, old_state) = (self.my_role, self.state);
        if self.am_buyer() && !self.state.precedes_for_buyer(new_state)
            || !self.am_buyer() && !self.state.precedes_for_seller(new_state)
        {
            return Err(ProtocolErrorKind::IllegalStateTransition {
                role,
                old_state,
                new_state,
            });
        }
        // FIXME: Persist the trade model together with its updated state. Assuming asynchronous
        //  persistence, we must ensure that the old state continues to be observed meanwhile.
        info!(trade_id = self.trade_id, %old_state, %new_state, "Updating trade state.");
        self.state = new_state;
        Ok(true)
    }
}

// ---- Operations: one for each request of the caller ----

impl TradeModel {
    /// Takes the key shares of the peer and the trade parameters. Afterwards the trader's half of
    /// the deposit, his addresses and his nonce shares exist.
    pub fn receive_key_shares(
        &mut self,
        wallet: &mut dyn ProtocolWalletApi,
        config: &EngineConfig,
        peers_key_shares: PeerData<ExchangedKeys<ByVal>>,
        params: TradeParams,
    ) -> Result<()> {
        self.atomically(|trade| {
            if let Some(stored_params) = &trade.received.params {
                // The request was processed before.
                let peers_key_shares = decoded(peers_key_shares)?;
                if !trade.keys.has_peers_key_shares(&peers_key_shares) {
                    return Err(peer_misbehaved(
                        "the key shares of the peer differ from the ones that it sent before",
                    ));
                }
                if *stored_params != params {
                    return Err(ProtocolErrorKind::ConflictingRequest("trade parameters").into());
                }
                return Ok(());
            }
            trade.require_state(&[TradeState::Init])?;
            params.validate(trade.network)?;
            let peers_key_shares = decoded(peers_key_shares)?;
            trade.check_peer_key_shares(&peers_key_shares)?;

            trade.set_peer_key_shares(&peers_key_shares);
            trade.set_params(&params, *config)?;
            trade.aggregate_key_shares(*config)?;
            trade.init_my_addresses(wallet)?;
            trade.init_my_half_deposit_psbt(wallet)?;
            trade.init_my_nonce_shares()?;
            trade.received.params = Some(params);
            trade.received.config = Some(*config);
            // The last step, because it changes the wallet: a request that is refused must not
            // leave coins reserved.
            trade.reserve_my_deposit_inputs(wallet)?;
            Ok(())
        })
    }

    /// Takes the peer's half of the deposit, addresses and nonce shares, and the redirection
    /// receivers of the caller. Afterwards all transactions of the trade are computed and the
    /// trader's partial signatures exist.
    pub fn receive_nonce_shares(
        &mut self,
        peers_message: PeerData<PeersNonceSharesMessage>,
        redirection_receivers: Vec<Receiver<NetworkUnchecked>>,
    ) -> Result<()> {
        self.atomically(|trade| {
            if let Some(stored) = &trade.received.nonce_shares {
                // The request was processed before.
                let received =
                    ReceivedNonceShares::new(decoded(peers_message)?, redirection_receivers);
                if stored.half_deposit_psbt != received.half_deposit_psbt
                    || stored.addresses != received.addresses
                    || stored.nonce_shares != received.nonce_shares
                {
                    return Err(peer_misbehaved(
                        "the half deposit, the addresses or the nonce shares of the peer differ \
                         from the ones that it sent before",
                    ));
                }
                if stored.redirection_receivers != received.redirection_receivers {
                    return Err(
                        ProtocolErrorKind::ConflictingRequest("redirection receivers").into(),
                    );
                }
                return Ok(());
            }
            trade.require_state(&[TradeState::Init])?;
            if trade.received.params.is_none() {
                return Err(ProtocolErrorKind::OutOfOrder(
                    "the key shares of the peer are missing",
                )
                .into());
            }

            // Data of the caller.
            trade.set_redirection_receivers(&redirection_receivers)?;
            trade.check_redirect_tx_params()?;

            // Data of the peer.
            let received = ReceivedNonceShares::new(decoded(peers_message)?, redirection_receivers);
            let [warning_tx_fee_bump, redirect_tx_fee_bump, claim_tx_payout] =
                received.addresses.clone();
            let peers_addresses = ExchangedAddresses {
                warning_tx_fee_bump,
                redirect_tx_fee_bump,
                claim_tx_payout,
            }
            .require_network(trade.network)
            .or_peer_misbehaved(
                "an address of the peer is not valid for the network of the trade",
            )?;
            trade.set_peer_half_deposit_psbt(received.half_deposit_psbt.clone());
            trade
                .compute_unsigned_deposit_tx()
                .or_peer_misbehaved("the peer's half of the deposit transaction is invalid")?;
            trade.set_peer_addresses(peers_addresses);
            trade.compute_unsigned_prepared_txs()?;
            if let Some(reason) = trade.find_repeated_nonce_share(&received.nonce_shares)? {
                return Err(peer_misbehaved(reason));
            }
            trade.set_peer_nonce_shares(&received.nonce_shares);
            trade.aggregate_nonce_shares()?;
            trade.sign_partial()?;
            trade.received.nonce_shares = Some(received);
            Ok(())
        })
    }

    /// Takes the partial signatures of the peer, completes the trader's own pre-signed
    /// transactions and signs the trader's inputs of the deposit transaction.
    pub fn receive_partial_signatures(
        &mut self,
        wallet: &mut dyn ProtocolWalletApi,
        peers_partial_signatures: PeerData<ExchangedSigs<ByVal>>,
    ) -> Result<()> {
        self.atomically(|trade| {
            if let Some(stored) = &trade.received.partial_signatures {
                // The request was processed before.
                let received =
                    trade.received_partial_signatures(&decoded(peers_partial_signatures)?)?;
                if *stored != received {
                    return Err(peer_misbehaved(
                        "the partial signatures of the peer differ from the ones that it sent \
                         before",
                    ));
                }
                return Ok(());
            }
            trade.require_state(&[TradeState::Init])?;
            if trade.received.nonce_shares.is_none() {
                return Err(ProtocolErrorKind::OutOfOrder(
                    "the nonce shares of the peer are missing",
                )
                .into());
            }

            let received =
                trade.received_partial_signatures(&decoded(peers_partial_signatures)?)?;
            if let Some((_, sighash)) = received.swap_tx {
                trade.sign_swap_tx_input_partial(sighash)?;
            }
            trade.set_peer_partial_signatures_on_my_txs(&received);
            let config = trade.received.config.unwrap_or_default();
            trade
                .aggregate_partial_signatures()
                .or_peer_misbehaved(&format!(
                    "a partial signature of the peer is not valid for the transactions that we \
                     computed (the peer runs modified software, or the two traders use different \
                     trade parameters or different lock times; our configuration: {config})"
                ))?;
            trade.compute_my_signed_prepared_txs()?;
            trade.deposit_signing_height = Some(wallet.chain_tip_height());
            trade.sign_deposit_psbt(wallet)?;
            trade.received.partial_signatures = Some(received);
            Ok(())
        })
    }

    /// The partial signatures of the peer that the trader needs in his role.
    fn received_partial_signatures(
        &self,
        peers_partial_signatures: &ExchangedSigs<ByVal>,
    ) -> Result<ReceivedPartialSignatures, Failure> {
        let swap_tx_input = if self.am_buyer() {
            let missing = "the seller sent no signature hash or no partial signature for the swap \
                           transaction";
            Some(
                peers_partial_signatures
                    .swap_tx_input_partial_signature
                    .zip(peers_partial_signatures.swap_tx_input_sighash)
                    .ok_or_else(|| peer_misbehaved(missing))?,
            )
        } else {
            // The buyer hands out this signature only when he has started the payment.
            None
        };
        Ok(ReceivedPartialSignatures {
            warning_tx_buyer: peers_partial_signatures
                .peers_warning_tx_buyer_input_partial_signature,
            warning_tx_seller: peers_partial_signatures
                .peers_warning_tx_seller_input_partial_signature,
            redirect_tx: peers_partial_signatures.peers_redirect_tx_input_partial_signature,
            swap_tx: swap_tx_input,
        })
    }

    /// Takes the deposit PSBT with the signatures of the peer and returns the signed deposit
    /// transaction. The transaction is recorded in the wallet as unconfirmed, so the wallet knows
    /// that its coins are spent before a block confirms it. The caller sends the transaction to
    /// the network.
    pub fn receive_deposit_psbt(
        &mut self,
        wallet: &mut dyn ProtocolWalletApi,
        peers_deposit_psbt: PeerData<Psbt>,
    ) -> Result<Transaction> {
        self.atomically(|trade| {
            // Every state after `Init` means that the trader signed the deposit transaction.
            if trade.state == TradeState::Init {
                return Err(ProtocolErrorKind::NotAllowedInState(trade.state).into());
            }
            let peers_deposit_psbt = decoded(peers_deposit_psbt)?;
            let txid = trade.deposit_tx.builder.psbt()?.unsigned_tx.compute_txid();
            let peers_txid = peers_deposit_psbt.unsigned_tx.compute_txid();
            if txid != peers_txid {
                return Err(peer_misbehaved(format!(
                    "the deposit PSBT of the peer is for the transaction {peers_txid}, not for \
                     the deposit transaction {txid}"
                )));
            }
            trade
                .deposit_tx
                .builder
                .combine_psbts(peers_deposit_psbt)
                .or_peer_misbehaved("the deposit PSBT of the peer cannot be combined with ours")?;
            let deposit_tx =
                trade.deposit_tx.builder.signed_tx().or_peer_misbehaved(
                    "the deposit PSBT of the peer does not sign all its inputs",
                )?;
            wallet.record_transaction(&deposit_tx)?;
            Ok(deposit_tx)
        })
    }

    /// The buyer reports that he started the payment. Afterwards his partial signature for the
    /// swap transaction is part of [`Self::my_partial_signatures_on_peer_txs`].
    pub fn start_payment(&mut self) -> Result<()> {
        self.atomically(|trade| {
            trade.require_buyer()?;
            if trade.received.payment_started {
                return Ok(());
            }
            trade.require_state(&[TradeState::Deposit])?;
            trade.require_confirmed_deposit()?;
            // Make sure we have the adaptor signature of the swap transaction before proceeding:
            trade.swap_tx.input_sig_ctx.aggregated_sig()?;
            trade.update_state(TradeState::BuyerReadyToRelease)?;
            trade.received.payment_started = true;
            Ok(())
        })
    }

    /// The seller takes the buyer's partial signature for the swap transaction.
    pub fn receive_swap_tx_signature(
        &mut self,
        signature: PeerData<PartialSignature>,
    ) -> Result<()> {
        self.atomically(|trade| {
            trade.require_seller()?;
            if let Some(stored) = trade.received.swap_tx_signature {
                if stored != decoded(signature)? {
                    return Err(peer_misbehaved(
                        "the buyer's partial signature for the swap transaction differs from the \
                         one that he sent before",
                    ));
                }
                return Ok(());
            }
            trade.require_state(&[TradeState::Deposit])?;
            let signature = decoded(signature)?;
            trade.swap_tx.input_sig_ctx.set_peers_partial_sig(signature);
            trade
                .swap_tx
                .input_sig_ctx
                .aggregate_partial_signatures()
                .or_peer_misbehaved(
                    "the buyer's partial signature for the swap transaction is invalid",
                )?;
            trade.update_state(TradeState::BuyerReadyToRelease)?;
            trade.received.swap_tx_signature = Some(signature);
            Ok(())
        })
    }

    /// The seller confirms the payment. Returns his secret key share for the buyer's payout.
    pub fn release_payout(&mut self) -> Result<Scalar> {
        self.atomically(|trade| {
            trade.require_seller()?;
            match trade.state() {
                TradeState::BuyerReadyToRelease => {
                    trade.require_confirmed_deposit()?;
                    // After the release the buyer holds the key of his payout. If he then
                    // publishes his warning transaction, the seller cannot use the penalty. The
                    // seller is safe only if his swap transaction confirms before the buyer's
                    // warning transaction becomes valid. The margin is the time for that (F6).
                    let remaining = trade.blocks_before_warning_is_valid()?;
                    let margin = u32::from(trade.config()?.release_safety_margin());
                    if remaining < margin {
                        return Err(ProtocolErrorKind::ChainCondition(format!(
                            "{remaining} blocks remain before the buyer's warning transaction \
                             becomes valid, which is less than the release safety margin of \
                             {margin} blocks"
                        ))
                        .into());
                    }
                    // Make sure we have the final Swap Tx before the key share leaves the engine:
                    trade.compute_signed_swap_tx()?;
                    trade.update_state(TradeState::SellerReadyToRelease)?;
                }
                TradeState::SellerReadyToRelease
                | TradeState::TradeClosed(ClosureType::Cooperative) => {}
                state => return Err(ProtocolErrorKind::NotAllowedInState(state).into()),
            }
            Ok(*trade.keys.peers_payout_ctx().my_key_share()?.prv_key()?)
        })
    }

    /// Closes the trade with the secret key share that the peer released. The trader now holds
    /// the complete key of his payout, and the key is imported into the wallet.
    pub fn close_with_key_share(
        &mut self,
        wallet: &mut dyn ProtocolWalletApi,
        peers_key_share: PeerData<Scalar>,
        penalty_tx_fee_rate: Option<FeeRate>,
    ) -> Result<()> {
        self.atomically(|trade| {
            let closed_before = trade.state == TradeState::TradeClosed(ClosureType::Cooperative);
            if !closed_before {
                if trade.am_buyer() {
                    trade.require_state(&[TradeState::BuyerReadyToRelease])?;
                } else {
                    trade.require_state(&[TradeState::SellerReadyToRelease])?;
                }
                trade.require_confirmed_deposit()?;
            }
            let peers_key_share = decoded(peers_key_share)?;
            let peers_pub_key_share = *trade.keys.my_payout_ctx().peers_key_share()?.pub_key();
            if closed_before {
                // The request was processed before.
                if peers_key_share.base_point_mul() != peers_pub_key_share {
                    return Err(peer_misbehaved(format!(
                        "the secret key share of the peer differs from the one that it sent \
                         before; it does not match the public key share {peers_pub_key_share}"
                    )));
                }
                return Ok(());
            }
            trade
                .keys
                .my_payout_ctx_mut()
                .set_peers_prv_key(peers_key_share)
                .or_peer_misbehaved(&format!(
                    "the secret key share of the peer does not match its public key share \
                     {peers_pub_key_share}"
                ))?;
            trade.sign_penalty_tx(penalty_tx_fee_rate)?;
            trade.close_trade(ClosureType::Cooperative)?;
            trade.import_my_payout_key(wallet)?;
            Ok(())
        })
    }

    /// The buyer closes the trade with a transaction that spends the seller's payout. He reads
    /// the seller's key share from its signature, and the complete key of his payout is imported
    /// into the wallet.
    pub fn close_with_swap_tx(
        &mut self,
        wallet: &mut dyn ProtocolWalletApi,
        swap_tx: &Transaction,
        penalty_tx_fee_rate: Option<FeeRate>,
    ) -> Result<()> {
        self.atomically(|trade| {
            trade.require_buyer()?;
            if trade.state == TradeState::TradeClosed(ClosureType::Forced) {
                return Ok(());
            }
            trade.require_state(&[
                TradeState::BuyerReadyToRelease,
                TradeState::CustomPayoutSigned,
            ])?;
            // The transaction comes from the chain or from the caller, not from a message of the
            // peer, so a transaction without the key share does not terminate the trade.
            trade
                .recover_seller_private_key_share_for_buyer_output(swap_tx)
                .map_err(|e| ProtocolErrorKind::UnusableSwapTx(e.to_string()))?;
            trade.sign_penalty_tx(penalty_tx_fee_rate)?;
            trade.close_trade(ClosureType::Forced)?;
            trade.import_my_payout_key(wallet)?;
            Ok(())
        })
    }

    /// The seller closes the trade without the buyer's key share. Afterwards
    /// [`Self::signed_swap_tx`] returns the swap transaction.
    pub fn close_by_force(&mut self) -> Result<()> {
        self.atomically(|trade| {
            trade.require_seller()?;
            if trade.state == TradeState::TradeClosed(ClosureType::Forced) {
                return Ok(());
            }
            trade.require_state(&[TradeState::SellerReadyToRelease])?;
            trade.close_trade(ClosureType::Forced)?;
            Ok(())
        })
    }

    /// Signs the custom payout transaction for the seller's amount before the fee and the fee
    /// rate of the request.
    pub fn sign_custom_payout(
        &mut self,
        wallet: &mut dyn ProtocolWalletApi,
        sellers_payout_amount_excluding_fee: Amount,
        fee_rate: FeeRate,
    ) -> Result<()> {
        self.atomically(|trade| {
            let request = (sellers_payout_amount_excluding_fee, fee_rate);
            if let Some(stored) = trade.received.custom_payout_request {
                // The engine never signs two different custom payout transactions for one trade.
                if stored != request {
                    return Err(ProtocolErrorKind::ConflictingRequest(
                        "amount or fee rate of the custom payout",
                    )
                    .into());
                }
                return Ok(());
            }
            trade.require_state(&[TradeState::Deposit, TradeState::BuyerReadyToRelease])?;
            trade.require_confirmed_deposit()?;
            trade
                .custom_payout_tx
                .builder
                .set_seller_payout_amount_excluding_fee(sellers_payout_amount_excluding_fee)
                .set_fee_rate(fee_rate);
            trade.compute_custom_payout_tx()?;
            trade.custom_payout_tx.builder.sign_partial(wallet)?;
            trade.update_state(TradeState::CustomPayoutSigned)?;
            trade.received.custom_payout_request = Some(request);
            Ok(())
        })
    }

    /// Takes the custom payout PSBT of the peer and returns the signed custom payout
    /// transaction.
    pub fn finalize_custom_payout(
        &mut self,
        wallet: &mut dyn ProtocolWalletApi,
        peers_custom_payout_psbt: PeerData<Psbt>,
    ) -> Result<Transaction> {
        self.atomically(|trade| {
            trade.require_state(&[
                TradeState::CustomPayoutSigned,
                TradeState::TradeClosed(ClosureType::Custom),
            ])?;
            let peers_custom_payout_psbt = decoded(peers_custom_payout_psbt)?;
            // A PSBT of the peer for another transaction is refused. It does not prove that the
            // peer misbehaved: the two callers can have used different amounts or fee rates.
            let expected = trade
                .custom_payout_tx
                .builder
                .psbt()?
                .unsigned_tx
                .compute_txid();
            let actual = peers_custom_payout_psbt.unsigned_tx.compute_txid();
            if expected != actual {
                return Err(ProtocolErrorKind::OtherCustomPayoutTx { expected, actual }.into());
            }
            trade
                .custom_payout_tx
                .builder
                .combine_psbts(peers_custom_payout_psbt)?;
            // Sign the custom payout PSBT again to finalize it:
            trade.custom_payout_tx.builder.sign_partial(wallet)?;
            // TODO: Check the signatures of the peer. A PSBT for our transaction with an invalid
            //  signature proves a modified client. The mock wallets of the tests sign with fixed
            //  signatures, so this check needs tests with real wallets.
            let Ok(custom_payout_tx) = trade.custom_payout_tx.builder.signed_tx() else {
                return Err(ProtocolErrorKind::MissingPeerSignature.into());
            };
            trade.close_trade(ClosureType::Custom)?;
            Ok(custom_payout_tx)
        })
    }
}

// ---- Coins and keys of the trade in the wallet ----

impl TradeModel {
    /// Ends a trade whose deposit transaction the trader has not signed, and releases the coins
    /// that were reserved for it. This is possible only in the state `Init`, also for a trade
    /// that was terminated. Afterwards the trade accepts no request that would continue it.
    pub fn abandon(&mut self, wallet: &mut dyn ProtocolWalletApi) -> Result<()> {
        let abandoned = TradeState::TradeClosed(ClosureType::Abandoned);
        if self.state == abandoned {
            return Ok(());
        }
        // In every later state the trader has signed the deposit transaction. The peer can
        // publish it, so the coins must stay reserved.
        self.require_state(&[TradeState::Init])?;
        for outpoint in &self.reserved_coins {
            wallet.unlock_utxo(*outpoint)?;
        }
        self.reserved_coins.clear();
        self.set_state(abandoned)?;
        Ok(())
    }

    /// Abandons the trade if it was terminated during the setup. The caller of the engine calls
    /// this after every request, because the engine has no wallet of its own to release the
    /// reserved coins at the moment of the termination.
    pub fn abandon_terminated_setup(&mut self, wallet: &mut dyn ProtocolWalletApi) -> Result<()> {
        if self.terminated.is_some() && self.state == TradeState::Init {
            self.abandon(wallet)?;
        }
        Ok(())
    }

    /// The coins of the wallet that are reserved for the trader's half of the deposit.
    pub fn reserved_coins(&self) -> &[OutPoint] {
        &self.reserved_coins
    }

    /// Locks the inputs of the trader's half of the deposit transaction in the wallet, so that
    /// no other trade and no payment selects them.
    fn reserve_my_deposit_inputs(&mut self, wallet: &mut dyn ProtocolWalletApi) -> Result<()> {
        let outpoints = self
            .my_half_deposit_psbt()?
            .unsigned_tx
            .input
            .iter()
            .map(|input| input.previous_output)
            .collect::<Vec<_>>();
        for (count, outpoint) in outpoints.iter().enumerate() {
            if let Err(error) = wallet.lock_utxo(*outpoint) {
                // Release what was locked so far. An error here cannot be handled further.
                for locked in &outpoints[..count] {
                    let _ = wallet.unlock_utxo(*locked);
                }
                return Err(error.into());
            }
        }
        self.reserved_coins = outpoints;
        Ok(())
    }

    /// Imports the complete key of the trader's payout into the wallet, with the script of the
    /// payout output, so that the wallet shows and can spend the payout.
    fn import_my_payout_key(&self, wallet: &mut dyn ProtocolWalletApi) -> Result<()> {
        let payout_key = *self.keys.my_payout_ctx().aggregated_key()?.prv_key()?;
        let [buyer_script_key, seller_script_key] = self.keys.multisig_script_keys()?;
        let tap_tree = script_paths::deposit_payout_tap_tree(buyer_script_key, seller_script_key)?;
        // The deposit transaction cannot be confirmed at or below the height at which the trader
        // signed it, so the wallet does not need to look for the payout in earlier blocks.
        let start_height = self.deposit_signing_height.unwrap_or(0);
        wallet.import_private_key_since(payout_key, Some(tap_tree), start_height)?;
        Ok(())
    }
}

// ---- The chain: what the engine observes and how it reacts ----

/// What the confirmed transactions say about the outputs of a trade.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Situation {
    /// A confirmed transaction that is not the deposit transaction spends an input of the
    /// deposit transaction. The deposit can never confirm.
    DepositConflict { confirmations: u32 },
    /// Both payouts are unspent.
    PayoutsUnspent,
    /// The warning transaction of one trader spent the payouts.
    Warning {
        of_buyer: bool,
        confirmations: u32,
        escrow: EscrowSpend,
    },
    /// The custom payout transaction spent the payouts.
    CustomPayout { confirmations: u32 },
    /// A transaction spent the seller's payout with the key path: the swap transaction, or a
    /// payment of the seller after a cooperative close.
    SellerPayoutSpent { spend: Spend },
    /// A transaction spent the buyer's payout, and the seller's payout is unspent: a payment of
    /// the buyer after he got the key.
    BuyerPayoutSpent { confirmations: u32 },
    /// A transaction that the engine cannot assign spent a payout.
    Unknown { txid: Txid, confirmations: u32 },
}

/// What happened to the escrow output of a confirmed warning transaction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum EscrowSpend {
    Unspent,
    /// The redirect transaction of the trader who did not publish the warning.
    Redirect {
        confirmations: u32,
    },
    /// A spend through the script path: the claim of the trader who published the warning.
    Claim {
        confirmations: u32,
    },
    /// Another spend through the key path: the penalty of the trader who did not publish the
    /// warning.
    Penalty {
        confirmations: u32,
    },
}

impl Situation {
    /// The number of confirmations of the transaction after which no transaction of the trade
    /// can confirm any more, or 0 if no such transaction is confirmed.
    const fn closing_confirmations(&self) -> u32 {
        match self {
            Self::PayoutsUnspent => 0,
            Self::Warning { escrow, .. } => match *escrow {
                EscrowSpend::Unspent => 0,
                EscrowSpend::Redirect { confirmations }
                | EscrowSpend::Claim { confirmations }
                | EscrowSpend::Penalty { confirmations } => confirmations,
            },
            Self::SellerPayoutSpent { spend } => spend.confirmations,
            Self::DepositConflict { confirmations }
            | Self::CustomPayout { confirmations }
            | Self::BuyerPayoutSpent { confirmations }
            | Self::Unknown { confirmations, .. } => *confirmations,
        }
    }
}

impl TradeModel {
    /// Takes the facts about the chain, lets the engine perform the steps that it takes on its
    /// own, and derives the state of the trade from the confirmed transactions. Returns what
    /// changed.
    ///
    /// The caller passes the facts before every request for the trade and after every change of
    /// the chain, and then sends the transactions of [`Self::transactions_to_send`].
    pub fn observe_chain(
        &mut self,
        wallet: &mut dyn ProtocolWalletApi,
        facts: ChainFacts,
    ) -> Vec<TradeEvent> {
        let mut events = Vec::new();
        let before = self.state();
        self.chain = facts;
        let not_started = TradeState::Init;
        let abandoned = TradeState::TradeClosed(ClosureType::Abandoned);
        if self.state == not_started || self.state == abandoned {
            return events;
        }
        self.deposit_watch_start
            .get_or_insert(self.chain.tip_height);

        let situation = match self.situation() {
            Ok(situation) => situation,
            Err(error) => {
                events.push(TradeEvent::Unexpected(format!(
                    "the chain cannot be evaluated for the trade: {error}"
                )));
                return events;
            }
        };
        self.read_key_share_from_spend(wallet, &situation, &mut events);
        self.close_by_force_before_the_warning_is_valid(&situation);

        let observed = self.state_from(&situation);
        self.observed_state = (observed != self.state).then_some(observed);
        let after = self.state();
        if after != before {
            info!(trade_id = self.trade_id, from = %before, to = %after, "The chain changed the trade state.");
            events.push(TradeEvent::StateChanged {
                from: before,
                to: after,
            });
            if after == TradeState::TradeClosed(ClosureType::DepositAborted) {
                self.release_reserved_coins(wallet, &mut events);
            }
            if let Some(loss) = self.loss(after) {
                error!(trade_id = self.trade_id, loss, "The trader lost funds.");
                events.push(TradeEvent::Loss(loss));
            }
        }
        if let Situation::Unknown { txid, .. } = situation {
            events.push(TradeEvent::Unexpected(format!(
                "the transaction {txid} spent a payout of the deposit transaction, and it is no \
                 transaction of the trade"
            )));
        }
        events
    }

    /// The facts about the chain that the engine was told last.
    pub const fn chain_facts(&self) -> &ChainFacts {
        &self.chain
    }

    /// Whether nothing can change for the trade any more: it was abandoned, or the transaction
    /// that closed it has [`MIN_CONFIRMATIONS`] confirmations. A trade that was closed
    /// cooperatively is final when a transaction that spends a payout has that depth, because
    /// then no warning transaction can confirm any more.
    pub fn is_final(&self) -> bool {
        if self.state == TradeState::TradeClosed(ClosureType::Abandoned) {
            return true;
        }
        if !matches!(self.state(), TradeState::TradeClosed(_)) {
            return false;
        }
        let confirmations = self
            .situation()
            .map_or(0, |situation| situation.closing_confirmations());
        confirmations >= u32::from(MIN_CONFIRMATIONS)
    }

    /// The trader decides to publish his warning transaction. Afterwards
    /// [`Self::transactions_to_send`] names it, until a warning transaction is confirmed.
    ///
    /// The request is refused unless the trade is open, the deposit transaction has as many
    /// confirmations as the warning lock time, and both payouts are unspent. A terminated trade
    /// keeps this action, because the trader can perform it alone.
    pub fn start_warning(&mut self) -> Result<Transaction> {
        let own_warning = if self.am_buyer() {
            TradeState::BuyersWarning
        } else {
            TradeState::SellersWarning
        };
        if self.state == own_warning {
            // The request was processed before.
            return Ok(self.my_txs().warning.builder.signed_tx()?.clone());
        }
        self.require_state(&[
            TradeState::Deposit,
            TradeState::BuyerReadyToRelease,
            TradeState::CustomPayoutSigned,
        ])?;
        let remaining = self.blocks_before_warning_is_valid()?;
        if remaining > 0 {
            return Err(ProtocolErrorKind::ChainCondition(format!(
                "the warning transaction is valid in {remaining} blocks: the deposit transaction \
                 has {} confirmations, and the warning lock time is {} blocks",
                self.chain.deposit_confirmations,
                self.config()?.warning_lock_time_blocks()
            )));
        }
        if self.situation()? != Situation::PayoutsUnspent {
            return Err(ProtocolErrorKind::ChainCondition(
                "a payout of the deposit transaction is spent".to_owned(),
            ));
        }
        let warning_tx = self.my_txs().warning.builder.signed_tx()?.clone();
        self.set_state(own_warning)?;
        Ok(warning_tx)
    }

    /// The transactions of the trader that should be on the network at the moment, according to
    /// the state of the trade and the facts about the chain. `fee_rate` is the fee rate of the
    /// transactions that the engine builds on its own.
    ///
    /// This is where the engine reacts to the chain (D7): it names the redirect or the penalty
    /// transaction when the warning transaction of the peer is confirmed, and the claim when the
    /// claim lock time of the trader's own warning has passed.
    pub fn transactions_to_send(&self, fee_rate: FeeRate) -> Vec<TxToSend> {
        let to_send = |kind, tx: Result<Transaction>| {
            tx.ok()
                .map(|tx| TxToSend { kind, tx })
                .into_iter()
                .collect()
        };
        let abandoned = TradeState::TradeClosed(ClosureType::Abandoned);
        if self.state == TradeState::Init || self.state == abandoned {
            return Vec::new();
        }
        let Ok(situation) = self.situation() else {
            return Vec::new();
        };
        if self.chain.deposit_confirmations == 0 {
            // Only the trader who holds all signatures can send the deposit transaction.
            let waiting = situation == Situation::PayoutsUnspent && !self.deposit_timed_out();
            return if waiting {
                to_send(TxKind::Deposit, self.signed_deposit_tx())
            } else {
                Vec::new()
            };
        }
        let seller_closed_by_force =
            !self.am_buyer() && self.state == TradeState::TradeClosed(ClosureType::Forced);
        match situation {
            Situation::PayoutsUnspent => {
                let own_warning = if self.am_buyer() {
                    TradeState::BuyersWarning
                } else {
                    TradeState::SellersWarning
                };
                if self.state == own_warning {
                    let valid = self.blocks_before_warning_is_valid().is_ok_and(|n| n == 0);
                    if valid {
                        return to_send(TxKind::Warning, self.my_signed_warning_tx().cloned());
                    }
                    Vec::new()
                } else if seller_closed_by_force {
                    to_send(TxKind::Swap, self.signed_swap_tx().cloned())
                } else if self.state == TradeState::TradeClosed(ClosureType::Custom) {
                    to_send(TxKind::CustomPayout, self.signed_custom_payout_tx())
                } else {
                    Vec::new()
                }
            }
            // The buyer spent his payout. The swap transaction of the seller is still valid.
            Situation::BuyerPayoutSpent { .. } if seller_closed_by_force => {
                to_send(TxKind::Swap, self.signed_swap_tx().cloned())
            }
            Situation::Warning {
                of_buyer,
                confirmations,
                escrow: EscrowSpend::Unspent,
            } => {
                if of_buyer == self.am_buyer() {
                    // The trader's own warning: the claim, when its lock time has passed.
                    let claim_lock_time = self
                        .config()
                        .map_or(u32::MAX, |config| config.claim_lock_time_blocks().into());
                    if confirmations >= claim_lock_time {
                        return to_send(TxKind::Claim, self.my_claim_tx(fee_rate));
                    }
                    Vec::new()
                } else if let Ok(penalty_tx) = self.penalty_tx.builder.signed_tx() {
                    // The trader holds the full key of the escrow of the peer's warning.
                    to_send(TxKind::Penalty, Ok(penalty_tx.clone()))
                } else {
                    to_send(TxKind::Redirect, self.my_signed_redirect_tx().cloned())
                }
            }
            _ => Vec::new(),
        }
    }

    /// Whether the deposit timeout has passed without a confirmation of the deposit
    /// transaction.
    fn deposit_timed_out(&self) -> bool {
        let Some(start) = self.deposit_watch_start else {
            return false;
        };
        let timeout = self
            .received
            .config
            .unwrap_or_default()
            .deposit_timeout()
            .into();
        self.chain.deposit_confirmations == 0
            && self.chain.tip_height.saturating_sub(start) > timeout
    }

    /// Sorts the confirmed spends of the outputs of the trade.
    fn situation(&self) -> Result<Situation> {
        let facts = &self.chain;
        let deposit_txid = *self.deposit_tx.builder.txid()?;
        if facts.deposit_confirmations == 0 {
            let conflict = self
                .deposit_inputs()?
                .into_iter()
                .filter_map(|(outpoint, _)| facts.spends.get(&outpoint))
                .find(|spend| spend.tx.compute_txid() != deposit_txid);
            return Ok(match conflict {
                Some(spend) => Situation::DepositConflict {
                    confirmations: spend.confirmations,
                },
                None => Situation::PayoutsUnspent,
            });
        }
        let buyer_payout = self.deposit_tx.builder.buyer_payout()?;
        let seller_payout = self.deposit_tx.builder.seller_payout()?;
        let buyer_payout_spend = facts.spends.get(&buyer_payout.outpoint);
        let seller_payout_spend = facts.spends.get(&seller_payout.outpoint);
        let Some(spend) = seller_payout_spend.or(buyer_payout_spend) else {
            return Ok(Situation::PayoutsUnspent);
        };
        let txid = spend.tx.compute_txid();
        let confirmations = spend.confirmations;

        for (of_buyer, txs, peer_txs) in [
            (true, &self.buyer_txs, &self.seller_txs),
            (false, &self.seller_txs, &self.buyer_txs),
        ] {
            if txid != *txs.warning.builder.txid()? {
                continue;
            }
            let escrow = txs.warning.builder.escrow()?;
            let escrow = match facts.spends.get(&escrow.outpoint) {
                None => EscrowSpend::Unspent,
                Some(spend) => {
                    let confirmations = spend.confirmations;
                    // The redirect transaction of the peer of the publisher spends this escrow.
                    if spend.tx.compute_txid() == *peer_txs.redirect.builder.txid()? {
                        EscrowSpend::Redirect { confirmations }
                    } else if spend.tx.find_key_spend_signature(&escrow).is_ok() {
                        EscrowSpend::Penalty { confirmations }
                    } else {
                        EscrowSpend::Claim { confirmations }
                    }
                }
            };
            return Ok(Situation::Warning {
                of_buyer,
                confirmations,
                escrow,
            });
        }
        let custom_payout_txid = self
            .custom_payout_tx
            .builder
            .psbt()
            .ok()
            .map(|psbt| psbt.unsigned_tx.compute_txid());
        if custom_payout_txid == Some(txid) {
            return Ok(Situation::CustomPayout { confirmations });
        }
        if let Some(spend) = seller_payout_spend {
            return Ok(
                if spend.tx.find_key_spend_signature(seller_payout).is_ok() {
                    Situation::SellerPayoutSpent {
                        spend: spend.clone(),
                    }
                } else {
                    Situation::Unknown {
                        txid,
                        confirmations,
                    }
                },
            );
        }
        Ok(Situation::BuyerPayoutSpent { confirmations })
    }

    /// The state that follows from the state that the requests gave the trade and from the
    /// confirmed transactions.
    fn state_from(&self, situation: &Situation) -> TradeState {
        use ClosureType::{
            BuyersClaim, BuyersPenaltyTx, BuyersRedirect, Cooperative, Custom, DepositAborted,
            Forced, SellersClaim, SellersPenaltyTx, SellersRedirect,
        };
        use TradeState::{BuyersWarning, SellerReadyToRelease, SellersWarning, TradeClosed};

        match *situation {
            Situation::DepositConflict { .. } => TradeClosed(DepositAborted),
            Situation::Warning {
                of_buyer,
                escrow: EscrowSpend::Unspent,
                ..
            } => {
                // After a cooperative close the trader answers with the penalty. The trade stays
                // closed until the penalty or the claim of the peer confirms.
                if self.state == TradeClosed(Cooperative) {
                    self.state
                } else if of_buyer {
                    BuyersWarning
                } else {
                    SellersWarning
                }
            }
            // The closure types are named after the trader who published the closing transaction:
            // the redirect and the penalty come from the peer of the publisher of the warning,
            // the claim from the publisher.
            Situation::Warning {
                of_buyer, escrow, ..
            } => TradeClosed(match (escrow, of_buyer) {
                (EscrowSpend::Redirect { .. }, true) => SellersRedirect,
                (EscrowSpend::Redirect { .. }, false) => BuyersRedirect,
                (EscrowSpend::Penalty { .. }, true) => SellersPenaltyTx,
                (EscrowSpend::Penalty { .. }, false) => BuyersPenaltyTx,
                (EscrowSpend::Claim { .. } | EscrowSpend::Unspent, true) => BuyersClaim,
                (EscrowSpend::Claim { .. } | EscrowSpend::Unspent, false) => SellersClaim,
            }),
            Situation::CustomPayout { .. } => TradeClosed(Custom),
            // The seller's own swap transaction. After a cooperative close it is a payment of
            // the seller, which changes nothing. The buyer reads the key share from the spend
            // (`read_key_share_from_spend`), which closes his trade for good.
            Situation::SellerPayoutSpent { .. }
                if !self.am_buyer() && self.state == SellerReadyToRelease =>
            {
                TradeClosed(Forced)
            }
            Situation::SellerPayoutSpent { .. }
            | Situation::BuyerPayoutSpent { .. }
            | Situation::PayoutsUnspent
            | Situation::Unknown { .. } => self.state,
        }
    }

    /// The buyer reads the seller's key share from a confirmed transaction that spends the
    /// seller's payout with the key path, and closes the trade with it. The buyer does not know
    /// the swap transaction, so every such transaction is examined.
    ///
    /// This close stays when a reorganisation removes the transaction: the buyer holds the key
    /// of his payout from then on.
    fn read_key_share_from_spend(
        &mut self,
        wallet: &mut dyn ProtocolWalletApi,
        situation: &Situation,
        events: &mut Vec<TradeEvent>,
    ) {
        let Situation::SellerPayoutSpent { spend } = situation else {
            return;
        };
        let waiting = [
            TradeState::BuyerReadyToRelease,
            TradeState::CustomPayoutSigned,
        ];
        if !self.am_buyer() || !waiting.contains(&self.state) {
            return;
        }
        let mut draft = self.clone();
        let result = draft
            .recover_seller_private_key_share_for_buyer_output(&spend.tx)
            .and_then(|()| draft.sign_penalty_tx(None))
            .and_then(|()| draft.close_trade(ClosureType::Forced))
            .and_then(|_| draft.import_my_payout_key(wallet));
        match result {
            Ok(()) => *self = draft,
            Err(error) => events.push(TradeEvent::Unexpected(format!(
                "the transaction {} spent the seller's payout, and the seller's key share cannot \
                 be read from it: {error}",
                spend.tx.compute_txid()
            ))),
        }
    }

    /// The seller publishes the swap transaction on his own when the number of blocks before
    /// the buyer's warning transaction becomes valid has fallen to the forced close margin of
    /// the configuration (D7, D23). He has released the key of the buyer's payout, so he must
    /// have his own payout before the buyer can publish the warning.
    fn close_by_force_before_the_warning_is_valid(&mut self, situation: &Situation) {
        if self.am_buyer()
            || self.state != TradeState::SellerReadyToRelease
            || *situation != Situation::PayoutsUnspent
            || self.chain.deposit_confirmations == 0
        {
            return;
        }
        let margin = self
            .received
            .config
            .unwrap_or_default()
            .forced_close_margin();
        if self
            .blocks_before_warning_is_valid()
            .is_ok_and(|remaining| remaining <= u32::from(margin))
        {
            info!(
                trade_id = self.trade_id,
                "The buyer did not answer the release in time. Closing by force."
            );
            if let Err(error) = self.close_trade(ClosureType::Forced) {
                error!(trade_id = self.trade_id, %error, "Could not close the trade by force.");
            }
        }
    }

    /// Releases the coins that are reserved for the deposit, when the deposit can never confirm.
    fn release_reserved_coins(
        &mut self,
        wallet: &mut dyn ProtocolWalletApi,
        events: &mut Vec<TradeEvent>,
    ) {
        for outpoint in std::mem::take(&mut self.reserved_coins) {
            if let Err(error) = wallet.unlock_utxo(outpoint) {
                events.push(TradeEvent::Unexpected(format!(
                    "the reserved coin {outpoint} could not be released: {error}"
                )));
            }
        }
    }

    /// The description of the loss, if `state` means that the peer took funds of the trader.
    fn loss(&self, state: TradeState) -> Option<String> {
        use ClosureType::{BuyersClaim, BuyersPenaltyTx, SellersClaim, SellersPenaltyTx};
        let TradeState::TradeClosed(closure_type) = state else {
            return None;
        };
        let by_peer = if self.am_buyer() {
            [SellersClaim, SellersPenaltyTx]
        } else {
            [BuyersClaim, BuyersPenaltyTx]
        };
        if !by_peer.contains(&closure_type) {
            return None;
        }
        Some(
            if closure_type == by_peer[0] {
                "the peer published his warning transaction and claimed all funds of the trade, \
                 because no redirect or penalty transaction of the trader was confirmed within \
                 the claim lock time"
            } else {
                "the peer took all funds of the trade with his penalty transaction, after the \
                 warning transaction of the trader was confirmed"
            }
            .to_owned(),
        )
    }
}

// ---- Data of the trade for the caller ----

impl TradeModel {
    pub fn my_key_shares(&self) -> Result<ExchangedKeys<'_, ByRef>> {
        Ok(ExchangedKeys {
            buyer_payout: self.keys.buyer_payout_ctx.my_key_share()?.pub_key(),
            seller_payout: self.keys.seller_payout_ctx.my_key_share()?.pub_key(),
            multisig_script: self
                .keys
                .my_multisig_script_key
                .as_ref()
                .ok_or(ProtocolErrorKind::MissingScriptKey)?,
        })
    }

    pub fn redirection_amount_msat(&self) -> Result<u64> {
        let split_input_amounts = [
            *self.deposit_tx.builder.trade_amount()?,
            *self.deposit_tx.builder.buyers_security_deposit()?,
            *self.deposit_tx.builder.sellers_security_deposit()?,
        ];
        let escrow_amount =
            WarningTxBuilder::escrow_amount(split_input_amounts, self.prepared_tx_fee_rate()?)?;

        Ok(RedirectTxBuilder::available_amount_msat(
            escrow_amount,
            self.prepared_tx_fee_rate()?,
        )?)
    }

    pub fn my_addresses(&self) -> Result<ExchangedAddresses<'_, ByRef>> {
        let my_txs = if self.am_buyer() {
            &self.buyer_txs
        } else {
            &self.seller_txs
        };
        Ok(ExchangedAddresses {
            warning_tx_fee_bump: my_txs.warning.builder.anchor_address()?,
            redirect_tx_fee_bump: my_txs.redirect.builder.anchor_address()?,
            claim_tx_payout: my_txs.payout_address()?,
        })
    }

    pub fn my_half_deposit_psbt(&self) -> Result<&Psbt> {
        Ok(if self.am_buyer() {
            self.deposit_tx.builder.buyers_half_psbt()?
        } else {
            self.deposit_tx.builder.sellers_half_psbt()?
        })
    }

    pub fn my_nonce_shares(&self) -> Result<ExchangedNonces<'_, ByRef>> {
        Ok(ExchangedNonces {
            swap_tx_input: self.swap_tx.input_sig_ctx.my_nonce_share()?,
            buyers_warning_tx_buyer_input: self
                .buyer_txs
                .warning
                .buyer_input_sig_ctx
                .my_nonce_share()?,
            buyers_warning_tx_seller_input: self
                .buyer_txs
                .warning
                .seller_input_sig_ctx
                .my_nonce_share()?,
            sellers_warning_tx_buyer_input: self
                .seller_txs
                .warning
                .buyer_input_sig_ctx
                .my_nonce_share()?,
            sellers_warning_tx_seller_input: self
                .seller_txs
                .warning
                .seller_input_sig_ctx
                .my_nonce_share()?,
            buyers_redirect_tx_input: self.buyer_txs.redirect.input_sig_ctx.my_nonce_share()?,
            sellers_redirect_tx_input: self.seller_txs.redirect.input_sig_ctx.my_nonce_share()?,
        })
    }

    /// The trader's partial signatures for the transactions of the peer. The buyer's partial
    /// signature for the swap transaction is included only after he started the payment.
    pub fn my_partial_signatures_on_peer_txs(&self) -> Result<ExchangedSigs<'_, ByRef>> {
        let peer_txs = if self.am_buyer() {
            &self.seller_txs
        } else {
            &self.buyer_txs
        };
        let ready_to_release = self.received.payment_started || !self.am_buyer();
        let contractual_txids_needed = self.state == TradeState::Init;

        Ok(ExchangedSigs {
            peers_warning_tx_buyer_input_partial_signature: peer_txs
                .warning
                .buyer_input_sig_ctx
                .my_partial_sig()?,
            peers_warning_tx_seller_input_partial_signature: peer_txs
                .warning
                .seller_input_sig_ctx
                .my_partial_sig()?,
            peers_redirect_tx_input_partial_signature: peer_txs
                .redirect
                .input_sig_ctx
                .my_partial_sig()?,
            swap_tx_input_partial_signature: self
                .swap_tx
                .input_sig_ctx
                .my_partial_sig()
                .ok()
                .filter(|_| ready_to_release),
            swap_tx_input_sighash: self.swap_tx.input_sighash.as_ref(),
            contractual_txids: self
                .contractual_txids()
                .ok()
                .filter(|_| contractual_txids_needed),
        })
    }

    pub fn deposit_psbt(&self) -> Result<&Psbt> {
        access!(self, _ if self.state >= Deposit)?;
        Ok(self.deposit_tx.builder.psbt()?)
    }

    pub fn signed_deposit_tx(&self) -> Result<Transaction> {
        access!(self, _ if self.state >= Deposit)?;
        Ok(self.deposit_tx.builder.signed_tx()?)
    }

    /// The ID of the deposit transaction. It is known as soon as the trader has signed the
    /// deposit, also for the trader who does not publish it.
    pub fn deposit_txid(&self) -> Result<Txid> {
        access!(self, _ if self.state >= Deposit)?;
        Ok(self.deposit_tx.builder.psbt()?.unsigned_tx.compute_txid())
    }

    /// The outputs that must be watched on the chain for this trade, from the moment the trader
    /// signed the deposit transaction:
    /// - the payouts of the buyer and of the seller in the deposit transaction, and the escrow
    ///   outputs of the buyer's and of the seller's warning transaction. Every transaction that can
    ///   end the trade spends one of them;
    /// - the inputs of the deposit transaction. A confirmed transaction that spends one of them and
    ///   is not the deposit transaction means that the deposit can never confirm.
    pub fn outputs_to_watch(&self) -> Result<Vec<(OutPoint, ScriptBuf)>> {
        access!(self, _ if self.state >= Deposit)?;
        let outputs = [
            self.deposit_tx.builder.buyer_payout()?.clone(),
            self.deposit_tx.builder.seller_payout()?.clone(),
            self.buyer_txs.warning.builder.escrow()?,
            self.seller_txs.warning.builder.escrow()?,
        ];
        let mut outputs = outputs
            .into_iter()
            .map(|output| (output.outpoint, output.prevout.script_pubkey))
            .collect::<Vec<_>>();
        outputs.extend(self.deposit_inputs()?);
        Ok(outputs)
    }

    /// The inputs of the deposit transaction, of both traders, with their scripts.
    fn deposit_inputs(&self) -> Result<Vec<(OutPoint, ScriptBuf)>> {
        let psbt = self.deposit_tx.builder.psbt()?;
        Ok(psbt
            .unsigned_tx
            .input
            .iter()
            .zip(&psbt.inputs)
            .filter_map(|(tx_in, input)| {
                let script = input.witness_utxo.as_ref()?.script_pubkey.clone();
                Some((tx_in.previous_output, script))
            })
            .collect())
    }

    pub fn my_private_key_share_for_peer_output(&self) -> Result<&Scalar> {
        access!(self, SellerReadyToRelease | TradeClosed(Cooperative))?;
        Ok(self.keys.peers_payout_ctx().my_key_share()?.prv_key()?)
    }

    pub fn signed_swap_tx(&self) -> Result<&Transaction> {
        access!(self, TradeClosed(Forced))?;
        Ok(self.swap_tx.builder.signed_tx()?)
    }

    pub fn signed_penalty_tx(&self) -> Result<&Transaction> {
        Ok(self.penalty_tx.builder.signed_tx()?)
    }

    pub fn custom_payout_psbt(&self) -> Result<&Psbt> {
        access!(self, CustomPayoutSigned | TradeClosed(Custom))?;
        Ok(self.custom_payout_tx.builder.psbt()?)
    }

    pub fn signed_custom_payout_tx(&self) -> Result<Transaction> {
        access!(self, CustomPayoutSigned | TradeClosed(Custom))?;
        Ok(self.custom_payout_tx.builder.signed_tx()?)
    }

    /// The trader's own warning transaction with all signatures. No operation publishes it yet.
    pub fn my_signed_warning_tx(&self) -> Result<&Transaction> {
        access!(self, _ if self.state >= Deposit)?;
        Ok(self.my_txs().warning.builder.signed_tx()?)
    }

    /// The trader's own redirect transaction with all signatures. It spends the escrow of the
    /// warning transaction of the peer. No operation publishes it yet.
    pub fn my_signed_redirect_tx(&self) -> Result<&Transaction> {
        access!(self, _ if self.state >= Deposit)?;
        Ok(self.my_txs().redirect.builder.signed_tx()?)
    }

    /// Builds and signs the trader's claim transaction for the fee rate `fee_rate`. It spends the
    /// escrow output of his own warning transaction through the script leaf of the escrow, with
    /// the claim lock time as sequence number, and pays to the trader's payout address.
    ///
    /// The trader signs alone, with his claim key: his own key share of his own payout. This key
    /// share never leaves the engine. A node accepts the transaction when the warning
    /// transaction has as many confirmations as the claim lock time.
    pub fn my_claim_tx(&self, fee_rate: FeeRate) -> Result<Transaction> {
        access!(self, _ if self.state >= Deposit)?;
        let config = self
            .received
            .config
            .ok_or(ProtocolErrorKind::OutOfOrder("the trade is not set up"))?;
        let claim_key_share = self.keys.my_payout_ctx().my_key_share()?;
        // The escrow of the trader's warning has the aggregated key of the payout of the peer
        // as its internal key.
        let internal_key = self
            .keys
            .peers_payout_ctx()
            .aggregated_key()?
            .pub_key()
            .to_public_key()
            .into();
        let (script, control_block) = script_paths::warning_escrow_claim_leaf(
            &internal_key,
            &claim_key_share.pub_key().to_public_key().into(),
            config.claim_lock_time(),
        )?;
        let secret_key = SecretKey::from_slice(&claim_key_share.prv_key()?.serialize())
            .expect("a scalar is a valid secret key");
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &secret_key);
        let my_txs = self.my_txs();
        Ok(build_claim_tx(
            &my_txs.warning.builder.escrow()?,
            my_txs.payout_address()?,
            config.claim_lock_time(),
            fee_rate,
            &script,
            &control_block,
            |sighash| {
                // The key of the script is the x-only form of the public key share. The signing
                // function negates the secret key when the public key has an odd Y coordinate.
                // The signature has no random part, so the engine builds the same transaction
                // every time it is asked for the same fee rate, and sends one claim, not many.
                let message = Message::from_digest(sighash.to_byte_array());
                Ok(taproot::Signature {
                    signature: secp.sign_schnorr_no_aux_rand(&message, &keypair),
                    sighash_type: TapSighashType::Default,
                })
            },
        )?)
    }
}

// ---- Steps of the operations ----

impl TradeModel {
    const fn my_txs(&self) -> &ArbitrationTxs {
        if self.am_buyer() {
            &self.buyer_txs
        } else {
            &self.seller_txs
        }
    }

    const fn my_txs_mut(&mut self) -> &mut ArbitrationTxs {
        if self.am_buyer() {
            &mut self.buyer_txs
        } else {
            &mut self.seller_txs
        }
    }

    const fn peer_txs_mut(&mut self) -> &mut ArbitrationTxs {
        if self.am_buyer() {
            &mut self.seller_txs
        } else {
            &mut self.buyer_txs
        }
    }

    fn prepared_tx_fee_rate(&self) -> Result<FeeRate> {
        Ok(*self.swap_tx.builder.fee_rate()?)
    }

    /// Checks the rules for the key shares of the peer: they differ from each other and from
    /// our own key shares, and the script key differs from our script key.
    fn check_peer_key_shares(&self, peers: &ExchangedKeys<ByVal>) -> Result<(), Failure> {
        let mine = self.my_key_shares()?;
        if peers.buyer_payout == peers.seller_payout {
            return Err(peer_misbehaved(format!(
                "the key shares of the peer for the two payouts are equal ({})",
                peers.buyer_payout
            )));
        }
        for (peers_key_share, payout) in [
            (peers.buyer_payout, "buyer's"),
            (peers.seller_payout, "seller's"),
        ] {
            if peers_key_share == *mine.buyer_payout || peers_key_share == *mine.seller_payout {
                return Err(peer_misbehaved(format!(
                    "the key share of the peer for the {payout} payout equals one of our own key \
                     shares ({peers_key_share})"
                )));
            }
        }
        if peers.multisig_script == *mine.multisig_script {
            return Err(peer_misbehaved(format!(
                "the script key of the peer equals our own script key ({})",
                peers.multisig_script
            )));
        }
        Ok(())
    }

    fn set_peer_key_shares(&mut self, keys: &ExchangedKeys<ByVal>) {
        self.keys
            .buyer_payout_ctx
            .set_peers_pub_key(keys.buyer_payout);
        self.keys
            .seller_payout_ctx
            .set_peers_pub_key(keys.seller_payout);
        self.keys
            .peers_multisig_script_key
            .get_or_insert(keys.multisig_script);
    }

    fn set_params(&mut self, params: &TradeParams, config: EngineConfig) -> Result<()> {
        self.deposit_tx
            .builder
            .set_trade_amount(params.trade_amount)
            .set_buyers_security_deposit(params.buyers_security_deposit)
            .set_sellers_security_deposit(params.sellers_security_deposit)
            .set_fee_rate(params.deposit_tx_fee_rate)
            .set_trade_fee_receivers(
                params
                    .trade_fee_receiver
                    .clone()
                    .map(|receiver| receiver.require_network(self.network))
                    .transpose()?
                    .into_iter()
                    .collect(),
            );
        for txs in [&mut self.buyer_txs, &mut self.seller_txs] {
            txs.warning
                .builder
                .set_lock_time(config.warning_lock_time());
            txs.warning
                .builder
                .set_fee_rate(params.prepared_tx_fee_rate);
        }
        self.swap_tx
            .builder
            .set_fee_rate(params.prepared_tx_fee_rate);
        Ok(())
    }

    fn aggregate_key_shares(&mut self, config: EngineConfig) -> Result<()> {
        let network = self.network;
        self.keys.buyer_payout_ctx.aggregate_pub_key_shares()?;
        self.keys.seller_payout_ctx.aggregate_pub_key_shares()?;

        let [buyer_pub_key, seller_pub_key] = self.keys.multisig_script_keys()?;
        let [buyer_internal_key, seller_internal_key] = self.keys.internal_keys()?;

        let deposit_merkle_root =
            script_paths::deposit_payout_merkle_root(buyer_pub_key, seller_pub_key)?;
        let buyer_payout_tweaked_key_ctx = self
            .keys
            .buyer_payout_ctx
            .with_taproot_tweak(Some(&deposit_merkle_root))?;
        let seller_payout_tweaked_key_ctx = self
            .keys
            .seller_payout_ctx
            .with_taproot_tweak(Some(&deposit_merkle_root))?;

        self.deposit_tx
            .builder
            .set_buyer_payout_address(buyer_payout_tweaked_key_ctx.p2tr_address(network))
            .set_seller_payout_address(seller_payout_tweaked_key_ctx.p2tr_address(network));
        self.custom_payout_tx
            .builder
            .set_buyer_input_descriptor(script_paths::deposit_payout_descriptor(
                &buyer_internal_key,
                buyer_pub_key,
                seller_pub_key,
            )?)
            .set_seller_input_descriptor(script_paths::deposit_payout_descriptor(
                &seller_internal_key,
                buyer_pub_key,
                seller_pub_key,
            )?);

        self.buyer_txs
            .warning
            .buyer_input_sig_ctx
            .set_tweaked_key_ctx(buyer_payout_tweaked_key_ctx.clone());
        self.seller_txs
            .warning
            .buyer_input_sig_ctx
            .set_tweaked_key_ctx(buyer_payout_tweaked_key_ctx);
        self.swap_tx
            .input_sig_ctx
            .set_adaptor_point(*self.keys.adaptor_key_share()?.pub_key())?;
        self.swap_tx
            .input_sig_ctx
            .set_tweaked_key_ctx(seller_payout_tweaked_key_ctx.clone());
        self.buyer_txs
            .warning
            .seller_input_sig_ctx
            .set_tweaked_key_ctx(seller_payout_tweaked_key_ctx.clone());
        self.seller_txs
            .warning
            .seller_input_sig_ctx
            .set_tweaked_key_ctx(seller_payout_tweaked_key_ctx);
        self.set_warning_escrow_keys(config)
    }

    /// Computes the keys and the addresses of the escrow outputs of both warning transactions.
    /// The escrow of a trader's warning has the internal key of the payout of the peer, and a
    /// script leaf with the trader's own key share as claim key.
    fn set_warning_escrow_keys(&mut self, config: EngineConfig) -> Result<()> {
        let network = self.network;
        let [buyer_claim_merkle_root, seller_claim_merkle_root] =
            self.keys.claim_key_shares()?.map(|p| {
                script_paths::warning_escrow_merkle_root(
                    &p.pub_key().to_public_key().into(),
                    config.claim_lock_time(),
                )
            });
        let buyers_warning_escrow_tweaked_key_ctx = self
            .keys
            .seller_payout_ctx
            .with_taproot_tweak(Some(&buyer_claim_merkle_root?))?;
        let sellers_warning_escrow_tweaked_key_ctx = self
            .keys
            .buyer_payout_ctx
            .with_taproot_tweak(Some(&seller_claim_merkle_root?))?;

        let buyers_warning_escrow_address =
            buyers_warning_escrow_tweaked_key_ctx.p2tr_address(network);
        let sellers_warning_escrow_address =
            sellers_warning_escrow_tweaked_key_ctx.p2tr_address(network);
        self.buyer_txs
            .warning
            .builder
            .set_escrow_address(buyers_warning_escrow_address);
        self.seller_txs
            .warning
            .builder
            .set_escrow_address(sellers_warning_escrow_address);

        self.penalty_tx.input_sig_ctx.set_tweaked_key_ctx(
            if self.am_buyer() {
                &sellers_warning_escrow_tweaked_key_ctx
            } else {
                &buyers_warning_escrow_tweaked_key_ctx
            }
            .clone(),
        );

        self.seller_txs
            .redirect
            .input_sig_ctx
            .set_tweaked_key_ctx(buyers_warning_escrow_tweaked_key_ctx);
        self.buyer_txs
            .redirect
            .input_sig_ctx
            .set_tweaked_key_ctx(sellers_warning_escrow_tweaked_key_ctx);
        Ok(())
    }

    fn init_my_addresses(&mut self, wallet: &mut dyn ProtocolWalletApi) -> Result<()> {
        if !self.am_buyer() {
            self.swap_tx
                .builder
                .set_payout_address(wallet.new_address()?);
        }
        let my_txs = self.my_txs_mut();
        my_txs
            .warning
            .builder
            .set_anchor_address(wallet.new_address()?);
        my_txs
            .redirect
            .builder
            .set_anchor_address(wallet.new_address()?);
        let payout_address = wallet.new_address()?;
        my_txs.payout_address = Some(payout_address.clone());
        self.penalty_tx.builder.set_payout_address(payout_address);
        Ok(())
    }

    fn init_my_half_deposit_psbt(&mut self, wallet: &mut dyn ProtocolWalletApi) -> Result<()> {
        if self.am_buyer() {
            self.deposit_tx
                .builder
                .init_buyers_half_psbt(wallet, &mut rand::rng())?;
        } else {
            self.deposit_tx
                .builder
                .init_sellers_half_psbt(wallet, &mut rand::rng())?;
        }
        Ok(())
    }

    fn init_my_nonce_shares(&mut self) -> Result<()> {
        for ctx in self.all_sig_ctxs_mut() {
            ctx.init_my_nonce_share()?;
        }
        Ok(())
    }

    fn set_redirection_receivers(
        &mut self,
        receivers: &[Receiver<NetworkUnchecked>],
    ) -> Result<()> {
        let mut checked_receivers = Vec::with_capacity(receivers.len());
        for receiver in receivers {
            checked_receivers.push(receiver.clone().require_network(self.network)?);
        }
        let receivers: ReceiverList = checked_receivers.into();
        self.buyer_txs
            .redirect
            .builder
            .set_receivers(receivers.clone());
        self.seller_txs.redirect.builder.set_receivers(receivers);
        Ok(())
    }

    fn check_redirect_tx_params(&self) -> Result<()> {
        let receivers = &self.buyer_txs.redirect.builder.receivers()?[..];
        let fee_rate = self.prepared_tx_fee_rate()?;
        let available_msat = self.redirection_amount_msat()?;
        let used_msat = Receiver::total_output_cost_msat(receivers, fee_rate, 1)?;

        if used_msat > available_msat {
            return Err(ProtocolErrorKind::InsufficientRedirectionFunds {
                available_msat,
                used_msat,
            });
        }
        if used_msat.saturating_add(999) < available_msat {
            return Err(ProtocolErrorKind::ExcessRedirectionFunds {
                available_msat,
                used_msat,
            });
        }
        Ok(())
    }

    fn set_peer_half_deposit_psbt(&mut self, half_deposit_psbt: Psbt) {
        if self.am_buyer() {
            self.deposit_tx
                .builder
                .set_sellers_half_psbt(half_deposit_psbt);
        } else {
            self.deposit_tx
                .builder
                .set_buyers_half_psbt(half_deposit_psbt);
        }
    }

    fn compute_unsigned_deposit_tx(&mut self) -> Result<()> {
        self.deposit_tx.builder.compute_unsigned_tx()?;
        Ok(())
    }

    fn set_peer_addresses(&mut self, addresses: ExchangedAddresses<ByVal>) {
        let peer_txs = self.peer_txs_mut();
        peer_txs
            .warning
            .builder
            .set_anchor_address(addresses.warning_tx_fee_bump);
        peer_txs
            .redirect
            .builder
            .set_anchor_address(addresses.redirect_tx_fee_bump);
        peer_txs.payout_address = Some(addresses.claim_tx_payout);
    }

    fn compute_unsigned_prepared_txs(&mut self) -> Result<()> {
        if !self.am_buyer() {
            // Only the seller has all the params necessary to compute the unsigned swap tx.
            self.swap_tx
                .builder
                .set_input(self.deposit_tx.builder.seller_payout()?.clone());
            self.swap_tx.builder.compute_unsigned_tx()?;
        }
        self.buyer_txs
            .compute_unsigned_warning_tx(&self.deposit_tx)?;
        self.seller_txs
            .compute_unsigned_warning_tx(&self.deposit_tx)?;
        self.buyer_txs
            .compute_unsigned_redirect_tx(&self.seller_txs)?;
        self.seller_txs
            .compute_unsigned_redirect_tx(&self.buyer_txs)?;
        Ok(())
    }

    fn contractual_txids(&self) -> Result<ContractualTxids> {
        Ok(ContractualTxids {
            deposit: *self.deposit_tx.builder.txid()?,
            buyers_warning: *self.buyer_txs.warning.builder.txid()?,
            sellers_warning: *self.seller_txs.warning.builder.txid()?,
            buyers_redirect: *self.buyer_txs.redirect.builder.txid()?,
            sellers_redirect: *self.seller_txs.redirect.builder.txid()?,
        })
    }

    const fn all_sig_ctxs_mut(&mut self) -> [&mut SigCtx; 7] {
        [
            &mut self.swap_tx.input_sig_ctx,
            &mut self.buyer_txs.warning.buyer_input_sig_ctx,
            &mut self.buyer_txs.warning.seller_input_sig_ctx,
            &mut self.seller_txs.warning.buyer_input_sig_ctx,
            &mut self.seller_txs.warning.seller_input_sig_ctx,
            &mut self.buyer_txs.redirect.input_sig_ctx,
            &mut self.seller_txs.redirect.input_sig_ctx,
        ]
    }

    /// Looks for a nonce share of the peer that equals one of our own nonce shares or an earlier
    /// nonce share of the peer. A nonce share belongs to exactly one signature. Returns a
    /// description of the first repeated one. The order of the nonce shares is the order of
    /// `all_sig_ctxs_mut`.
    fn find_repeated_nonce_share(
        &mut self,
        peers_nonce_shares: &[PubNonce],
    ) -> Result<Option<String>> {
        const SIGNATURES: [&str; 7] = [
            "swap transaction",
            "buyer's warning transaction (buyer input)",
            "buyer's warning transaction (seller input)",
            "seller's warning transaction (buyer input)",
            "seller's warning transaction (seller input)",
            "buyer's redirect transaction",
            "seller's redirect transaction",
        ];
        let sig_ctxs = self.all_sig_ctxs_mut();
        let mut my_nonce_shares = Vec::with_capacity(sig_ctxs.len());
        for sig_ctx in &sig_ctxs {
            my_nonce_shares.push(sig_ctx.my_nonce_share()?);
        }
        for (i, nonce_share) in peers_nonce_shares.iter().enumerate() {
            let other = if let Some(j) = my_nonce_shares.iter().position(|n| *n == nonce_share) {
                format!("our own nonce share for the {}", SIGNATURES[j])
            } else if let Some(j) = peers_nonce_shares[..i]
                .iter()
                .position(|n| n == nonce_share)
            {
                format!("the peer's nonce share for the {}", SIGNATURES[j])
            } else {
                continue;
            };
            return Ok(Some(format!(
                "the peer's nonce share for the {} equals {other} (nonce share {nonce_share})",
                SIGNATURES[i]
            )));
        }
        Ok(None)
    }

    fn set_peer_nonce_shares(&mut self, nonce_shares: &[PubNonce; 7]) {
        for (sig_ctx, nonce_share) in self.all_sig_ctxs_mut().into_iter().zip(nonce_shares) {
            sig_ctx.set_peers_nonce_share(nonce_share.clone());
        }
    }

    fn aggregate_nonce_shares(&mut self) -> Result<()> {
        for ctx in self.all_sig_ctxs_mut() {
            ctx.aggregate_nonce_shares()?;
        }
        Ok(())
    }

    fn sign_partial(&mut self) -> Result<()> {
        for txs in [&mut self.buyer_txs, &mut self.seller_txs] {
            txs.warning
                .buyer_input_sig_ctx
                .sign_partial(txs.warning.builder.buyer_input_sighash()?)?;
            txs.warning
                .seller_input_sig_ctx
                .sign_partial(txs.warning.builder.seller_input_sighash()?)?;
            txs.redirect
                .input_sig_ctx
                .sign_partial(txs.redirect.builder.input_sighash()?)?;
        }
        if !self.am_buyer() {
            // Unlike the other multisig sighashes, only the seller is able to independently compute
            // the swap-tx-input sighash. The buyer must wait for the next round, when the deposit
            // tx is signed, to partially sign the swap tx using the sighash passed by the seller.
            self.sign_swap_tx_input_partial(self.swap_tx.builder.input_sighash()?)?;
        }
        Ok(())
    }

    fn sign_swap_tx_input_partial(&mut self, sighash: TapSighash) -> Result<()> {
        let sighash = self.swap_tx.input_sighash.get_or_insert(sighash);
        self.swap_tx.input_sig_ctx.sign_partial(*sighash)?;
        Ok(())
    }

    fn set_peer_partial_signatures_on_my_txs(&mut self, sigs: &ReceivedPartialSignatures) {
        let my_txs = self.my_txs_mut();
        my_txs
            .warning
            .buyer_input_sig_ctx
            .set_peers_partial_sig(sigs.warning_tx_buyer);
        my_txs
            .warning
            .seller_input_sig_ctx
            .set_peers_partial_sig(sigs.warning_tx_seller);
        my_txs
            .redirect
            .input_sig_ctx
            .set_peers_partial_sig(sigs.redirect_tx);
        if let Some((signature, _)) = sigs.swap_tx {
            self.swap_tx.input_sig_ctx.set_peers_partial_sig(signature);
        }
    }

    fn aggregate_partial_signatures(&mut self) -> Result<()> {
        let am_buyer = self.am_buyer();
        let my_txs = self.my_txs_mut();
        my_txs
            .warning
            .buyer_input_sig_ctx
            .aggregate_partial_signatures()?;
        my_txs
            .warning
            .seller_input_sig_ctx
            .aggregate_partial_signatures()?;
        my_txs
            .redirect
            .input_sig_ctx
            .aggregate_partial_signatures()?;
        if am_buyer {
            // This forms a validated adaptor signature on the swap tx for the buyer, ensuring that
            // the seller's private key share is revealed if the swap tx is published. The seller
            // doesn't get the full adaptor signature (or the ordinary signature) until later on in
            // the trade, when the buyer confirms payment:
            self.swap_tx.input_sig_ctx.aggregate_partial_signatures()?;
        }
        Ok(())
    }

    fn compute_my_signed_prepared_txs(&mut self) -> Result<()> {
        use MaybeScalar::Zero;
        let my_txs = self.my_txs_mut();
        my_txs
            .warning
            .builder
            .set_buyer_input_signature(
                my_txs
                    .warning
                    .buyer_input_sig_ctx
                    .compute_taproot_signature(Zero)?,
            )
            .set_seller_input_signature(
                my_txs
                    .warning
                    .seller_input_sig_ctx
                    .compute_taproot_signature(Zero)?,
            )
            .compute_signed_tx()?;
        my_txs
            .redirect
            .builder
            .set_input_signature(
                my_txs
                    .redirect
                    .input_sig_ctx
                    .compute_taproot_signature(Zero)?,
            )
            .compute_signed_tx()?;
        Ok(())
    }

    fn sign_deposit_psbt(&mut self, wallet: &mut dyn ProtocolWalletApi) -> Result<bool> {
        // Check that we have all the prepared tx data we need:
        let my_txs = self.my_txs();
        my_txs.warning.builder.signed_tx()?;
        my_txs.redirect.builder.signed_tx()?;
        my_txs.payout_address()?;
        if self.am_buyer() {
            self.swap_tx.input_sig_ctx.aggregated_sig()?;
        } else {
            self.swap_tx.builder.unsigned_tx()?;
            self.swap_tx.input_sig_ctx.my_partial_sig()?;
        }
        self.penalty_tx.builder.payout_address()?;
        self.penalty_tx.input_sig_ctx.tweaked_key_ctx()?;
        // Now sign the Deposit Tx. It is CRITICAL that we don't share the PSBT or signed tx until
        // the trade state has successfully advanced past `Init` to `Deposit`.
        if self.am_buyer() {
            self.deposit_tx.builder.sign_buyer_inputs(wallet)?;
        } else {
            self.deposit_tx.builder.sign_seller_inputs(wallet)?;
        }
        self.update_state(TradeState::Deposit)
    }

    fn compute_signed_swap_tx(&mut self) -> Result<()> {
        let adaptor_secret = (*self.keys.adaptor_key_share()?.prv_key()?).into();
        self.swap_tx
            .builder
            .set_input_signature(
                self.swap_tx
                    .input_sig_ctx
                    .compute_taproot_signature(adaptor_secret)?,
            )
            .compute_signed_tx()?;
        Ok(())
    }

    fn recover_seller_private_key_share_for_buyer_output(
        &mut self,
        swap_tx: &Transaction,
    ) -> Result<()> {
        let swap_tx_input = self.deposit_tx.builder.seller_payout()?;
        let input_signature = swap_tx.find_key_spend_signature(swap_tx_input)?;
        let adaptor_secret = self
            .swap_tx
            .input_sig_ctx
            .reveal_adaptor_secret(input_signature)?;
        self.keys
            .buyer_payout_ctx
            .set_peers_prv_key(adaptor_secret)?;
        Ok(())
    }

    /// Signs the penalty transaction with the full key of the escrow of the peer's warning
    /// transaction. The trader holds this key when he holds the peer's secret key share.
    fn sign_penalty_tx(&mut self, fee_rate: Option<FeeRate>) -> Result<()> {
        self.penalty_tx
            .builder
            .set_fee_rate(fee_rate.unwrap_or(self.prepared_tx_fee_rate()?));
        let peers_warning_escrow = if self.am_buyer() {
            self.seller_txs.warning.builder.escrow()?
        } else {
            self.buyer_txs.warning.builder.escrow()?
        };
        self.penalty_tx
            .builder
            .set_input(peers_warning_escrow)
            .compute_unsigned_tx()?;
        self.keys.my_payout_ctx_mut().aggregate_prv_key_shares()?;
        self.penalty_tx.input_sig_ctx.sign_solo(
            self.penalty_tx.builder.input_sighash()?,
            *self.keys.my_payout_ctx().peers_key_share()?.prv_key()?,
        )?;
        self.penalty_tx
            .builder
            .set_input_signature(
                self.penalty_tx
                    .input_sig_ctx
                    .compute_taproot_signature(MaybeScalar::Zero)?,
            )
            .compute_signed_tx()?;
        Ok(())
    }

    fn close_trade(&mut self, closure_type: ClosureType) -> Result<bool> {
        match closure_type {
            ClosureType::Forced if !self.am_buyer() => {
                self.swap_tx.builder.signed_tx()?;
                // TODO: Make sure we are committed to broadcasting the Swap Tx.
            }
            ClosureType::Cooperative | ClosureType::Forced => {
                // The caller imports the key into the wallet (`import_my_payout_key`).
                self.keys.my_payout_ctx().aggregated_key()?.prv_key()?;
                self.penalty_tx.builder.signed_tx()?;
                // TODO: If forced closure, make sure we've seen the Swap Tx on the network.
            }
            ClosureType::Custom => {
                self.custom_payout_tx.builder.signed_tx()?;
                // TODO: Make sure we are committed to broadcasting the Custom Payout Tx, unless we
                //  have already seen it on the network.
            }
            // These closure types follow from confirmed transactions only. The engine derives
            // them from the chain (`observe_chain`). A trade is abandoned by `abandon`.
            ClosureType::Abandoned
            | ClosureType::DepositAborted
            | ClosureType::BuyersRedirect
            | ClosureType::SellersRedirect
            | ClosureType::BuyersClaim
            | ClosureType::SellersClaim
            | ClosureType::BuyersPenaltyTx
            | ClosureType::SellersPenaltyTx => {
                return Err(ProtocolErrorKind::UnsupportedClosureType(closure_type));
            }
        }
        // Not `update_state`: a terminated trade keeps the closes that the trader can perform
        // alone. The requests of a cooperative close are refused for it before they get here.
        self.set_state(TradeState::TradeClosed(closure_type))
    }

    fn compute_custom_payout_tx(&mut self) -> Result<()> {
        self.custom_payout_tx
            .builder
            .set_buyer_input(self.deposit_tx.builder.buyer_payout()?.clone())
            .set_seller_input(self.deposit_tx.builder.seller_payout()?.clone())
            .set_buyer_payout_address(self.buyer_txs.payout_address()?.clone())
            .set_seller_payout_address(self.seller_txs.payout_address()?.clone())
            .compute_unsigned_tx()?;
        Ok(())
    }
}

impl<'a> ExchangedAddresses<'a, ByVal, NetworkUnchecked> {
    fn require_network(
        self,
        required: Network,
    ) -> Result<ExchangedAddresses<'a, ByVal>, bdk_wallet::bitcoin::address::ParseError> {
        Ok(ExchangedAddresses {
            warning_tx_fee_bump: self.warning_tx_fee_bump.require_network(required)?,
            redirect_tx_fee_bump: self.redirect_tx_fee_bump.require_network(required)?,
            claim_tx_payout: self.claim_tx_payout.require_network(required)?,
        })
    }
}

impl ArbitrationTxs {
    fn compute_unsigned_warning_tx(&mut self, deposit_tx: &DepositTx) -> Result<()> {
        self.warning
            .builder
            .set_buyer_input(deposit_tx.builder.buyer_payout()?.clone())
            .set_seller_input(deposit_tx.builder.seller_payout()?.clone())
            .compute_unsigned_tx()?;
        Ok(())
    }

    fn compute_unsigned_redirect_tx(&mut self, peer: &Self) -> Result<()> {
        self.redirect
            .builder
            .set_input(peer.warning.builder.escrow()?.clone())
            .compute_unsigned_tx()?;
        Ok(())
    }

    fn payout_address(&self) -> Result<&Address> {
        self.payout_address
            .as_ref()
            .ok_or(ProtocolErrorKind::OutOfOrder(
                "the payout address is missing",
            ))
    }
}

impl Keys {
    const fn my_payout_ctx(&self) -> &KeyCtx {
        if self.am_buyer {
            &self.buyer_payout_ctx
        } else {
            &self.seller_payout_ctx
        }
    }

    const fn my_payout_ctx_mut(&mut self) -> &mut KeyCtx {
        if self.am_buyer {
            &mut self.buyer_payout_ctx
        } else {
            &mut self.seller_payout_ctx
        }
    }

    const fn peers_payout_ctx(&self) -> &KeyCtx {
        if self.am_buyer {
            &self.seller_payout_ctx
        } else {
            &self.buyer_payout_ctx
        }
    }

    /// Whether the key shares equal the stored key shares of the peer.
    fn has_peers_key_shares(&self, keys: &ExchangedKeys<ByVal>) -> bool {
        let is_stored = |ctx: &KeyCtx, key_share: &Point| {
            ctx.peers_key_share()
                .is_ok_and(|stored| stored.pub_key() == key_share)
        };
        is_stored(&self.buyer_payout_ctx, &keys.buyer_payout)
            && is_stored(&self.seller_payout_ctx, &keys.seller_payout)
            && self.peers_multisig_script_key == Some(keys.multisig_script)
    }

    fn adaptor_key_share(&self) -> Result<&KeyPair> {
        Ok(if self.am_buyer {
            self.buyer_payout_ctx.peers_key_share()?
        } else {
            self.buyer_payout_ctx.my_key_share()?
        })
    }

    fn claim_key_shares(&self) -> Result<[&KeyPair; 2]> {
        Ok(if self.am_buyer {
            [
                self.buyer_payout_ctx.my_key_share()?,
                self.seller_payout_ctx.peers_key_share()?,
            ]
        } else {
            [
                self.buyer_payout_ctx.peers_key_share()?,
                self.seller_payout_ctx.my_key_share()?,
            ]
        })
    }

    fn multisig_script_keys(&self) -> Result<[&XOnlyPublicKey; 2]> {
        (|| {
            Some(if self.am_buyer {
                [
                    self.my_multisig_script_key.as_ref()?,
                    self.peers_multisig_script_key.as_ref()?,
                ]
            } else {
                [
                    self.peers_multisig_script_key.as_ref()?,
                    self.my_multisig_script_key.as_ref()?,
                ]
            })
        })()
        .ok_or(ProtocolErrorKind::MissingScriptKey)
    }

    fn internal_keys(&self) -> Result<[XOnlyPublicKey; 2]> {
        Ok([
            self.buyer_payout_ctx.aggregated_key()?,
            self.seller_payout_ctx.aggregated_key()?,
        ]
        .map(|p| p.pub_key().to_public_key().into()))
    }
}

type Result<T, E = ProtocolErrorKind> = std::result::Result<T, E>;

#[derive(Error, Debug)]
#[error(transparent)]
#[non_exhaustive]
pub enum ProtocolErrorKind {
    #[error("missing script key")]
    MissingScriptKey,
    #[error("access denied")]
    AccessDenied,
    #[error("trade with id {0} already exists")]
    DuplicateTradeId(String),
    #[error("operation only available for {0}")]
    WrongRole(&'static str),
    #[error("request is not allowed in state {0}")]
    NotAllowedInState(TradeState),
    #[error("request is out of order: {0}")]
    OutOfOrder(&'static str),
    #[error("the state of the chain does not allow the request yet: {0}")]
    ChainCondition(String),
    #[error("request differs from an earlier request for this trade: {0}")]
    ConflictingRequest(&'static str),
    #[error("trade was terminated, because the peer did not follow the protocol: {0}")]
    TradeTerminated(String),
    #[error("the transaction does not reveal the seller's key share: {0}")]
    UnusableSwapTx(String),
    #[error(
        "the custom payout PSBT of the peer is for the transaction {actual}, not for {expected}"
    )]
    OtherCustomPayoutTx {
        expected: Txid,
        actual: Txid,
    },
    #[error("the custom payout PSBT of the peer does not contain its signatures")]
    MissingPeerSignature,
    #[error("unsupported closure type: {0:?}")]
    UnsupportedClosureType(ClosureType),
    #[error("illegal state transition for {role:?}: {old_state} -> {new_state}")]
    IllegalStateTransition {
        role: Role,
        old_state: TradeState,
        new_state: TradeState,
    },
    #[error(
        "insufficient redirection funds (available {available_msat} msat, used {used_msat} msat)"
    )]
    InsufficientRedirectionFunds {
        available_msat: u64,
        used_msat: u64,
    },
    #[error("excess redirection funds (available {available_msat} msat, used {used_msat} msat)")]
    ExcessRedirectionFunds {
        available_msat: u64,
        used_msat: u64,
    },
    TradeParams(#[from] crate::params::TradeParamsError),
    AddressParse(#[from] bdk_wallet::bitcoin::address::ParseError),
    Transaction(#[from] crate::transaction::TransactionErrorKind),
    Multisig(#[from] crate::multisig::MultisigErrorKind),
    Wallet(#[from] wallet::error::WalletErrorKind),
}

impl ProtocolErrorKind {
    /// Whether the error says that a request is refused because a precondition does not hold or
    /// because data of the caller is invalid. The trade continues, and the request can be
    /// repeated when the reason is gone.
    pub const fn is_refusal(&self) -> bool {
        matches!(
            self,
            Self::WrongRole(_)
                | Self::NotAllowedInState(_)
                | Self::OutOfOrder(_)
                | Self::ChainCondition(_)
                | Self::ConflictingRequest(_)
                | Self::UnusableSwapTx(_)
                | Self::OtherCustomPayoutTx { .. }
                | Self::MissingPeerSignature
                | Self::TradeParams(_)
                | Self::InsufficientRedirectionFunds { .. }
                | Self::ExcessRedirectionFunds { .. }
        )
    }
}

/// Unit tests of the trade engine without the gRPC service. Two trade models, one per trader,
/// exchange their data directly. The test IDs (G1, G4, ...) refer to section 9.4 of
/// `concept/CompletionPlan.md`, the D and F numbers to its decisions and findings.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::mocks::{mock_buyer_trade_wallet, mock_seller_trade_wallet};

    //noinspection SpellCheckingInspection
    const REDIRECTION_RECEIVERS: [(&str, u64); 3] = [
        (
            "bcrt1phc8m8vansnl4utths947mjquprw20puwrrdfrwx8akeeu2tqwklsnxsvf0",
            160_000,
        ),
        ("bcrt1qwk6p86mzqmstcsg99qlu2mhsp3766u68jktv6k", 80_000),
        ("2N2x2bA28AsLZZEHss4SjFoyToQV5YYZsJM", 15_055),
    ];

    // The same receivers with other amounts. The total amount is the same.
    //noinspection SpellCheckingInspection
    const OTHER_REDIRECTION_RECEIVERS: [(&str, u64); 3] = [
        (
            "bcrt1phc8m8vansnl4utths947mjquprw20puwrrdfrwx8akeeu2tqwklsnxsvf0",
            150_000,
        ),
        ("bcrt1qwk6p86mzqmstcsg99qlu2mhsp3766u68jktv6k", 90_000),
        ("2N2x2bA28AsLZZEHss4SjFoyToQV5YYZsJM", 15_055),
    ];

    pub(super) const ROLE_PAIRS: [(Role, Role); 4] = [
        (Role::BuyerAsTaker, Role::SellerAsMaker),
        (Role::BuyerAsMaker, Role::SellerAsTaker),
        (Role::SellerAsTaker, Role::BuyerAsMaker),
        (Role::SellerAsMaker, Role::BuyerAsTaker),
    ];

    /// One trader: his trade model and his wallet.
    pub(super) struct Trader {
        pub(super) model: TradeModel,
        pub(super) wallet: Box<dyn ProtocolWalletApi>,
    }

    fn params() -> TradeParams {
        TradeParams {
            trade_amount: Amount::from_sat(200_000),
            buyers_security_deposit: Amount::from_sat(30_000),
            sellers_security_deposit: Amount::from_sat(30_000),
            deposit_tx_fee_rate: FeeRate::from_sat_per_kwu(3_125),
            prepared_tx_fee_rate: FeeRate::from_sat_per_kwu(2_500),
            trade_fee_receiver: None,
        }
    }

    /// The configuration of the engines of both traders. Its lock times differ from the default
    /// values, so the tests show that the engine uses the configuration.
    pub(super) fn config() -> EngineConfig {
        EngineConfig::new(20, 10, 144, 6).expect("valid configuration")
    }

    fn receivers(receivers: [(&str, u64); 3]) -> Vec<Receiver<NetworkUnchecked>> {
        receivers
            .into_iter()
            .map(|(address, amount)| Receiver {
                address: address.parse().expect("valid address"),
                amount: Amount::from_sat(amount),
            })
            .collect()
    }

    fn new_trader(role: Role) -> Trader {
        new_trader_for_trade(role, "trade")
    }

    /// A trader for the trade `trade_id`. The key shares are derived from the seed of the mock
    /// wallet and the trade ID, so two traders with the same role and trade ID have the same key
    /// shares.
    fn new_trader_for_trade(role: Role, trade_id: &str) -> Trader {
        let mut wallet: Box<dyn ProtocolWalletApi> =
            if matches!(role, Role::BuyerAsMaker | Role::BuyerAsTaker) {
                Box::new(mock_buyer_trade_wallet())
            } else {
                Box::new(mock_seller_trade_wallet())
            };
        let model =
            TradeModel::new(trade_id.to_owned(), role, &mut *wallet).expect("trade can be created");
        Trader { model, wallet }
    }

    fn sent_key_shares(trader: &Trader) -> ExchangedKeys<'static, ByVal> {
        let key_shares = trader.model.my_key_shares().expect("key shares exist");
        ExchangedKeys {
            buyer_payout: *key_shares.buyer_payout,
            seller_payout: *key_shares.seller_payout,
            multisig_script: *key_shares.multisig_script,
        }
    }

    fn receive_key_shares(
        trader: &mut Trader,
        peers_key_shares: &ExchangedKeys<ByVal>,
    ) -> Result<()> {
        let peers_key_shares = ExchangedKeys {
            buyer_payout: peers_key_shares.buyer_payout,
            seller_payout: peers_key_shares.seller_payout,
            multisig_script: peers_key_shares.multisig_script,
        };
        trader.model.receive_key_shares(
            &mut *trader.wallet,
            &config(),
            Ok(peers_key_shares),
            params(),
        )
    }

    type SentNonceShares = PeersNonceSharesMessage;

    fn sent_nonce_shares(trader: &Trader) -> SentNonceShares {
        let model = &trader.model;
        let half_deposit_psbt = model.my_half_deposit_psbt().expect("PSBT exists").clone();
        let addresses = model.my_addresses().expect("addresses exist");
        let addresses = ExchangedAddresses {
            warning_tx_fee_bump: addresses.warning_tx_fee_bump.as_unchecked().clone(),
            redirect_tx_fee_bump: addresses.redirect_tx_fee_bump.as_unchecked().clone(),
            claim_tx_payout: addresses.claim_tx_payout.as_unchecked().clone(),
        };
        let nonces = model.my_nonce_shares().expect("nonce shares exist");
        let nonces = ExchangedNonces {
            swap_tx_input: nonces.swap_tx_input.clone(),
            buyers_warning_tx_buyer_input: nonces.buyers_warning_tx_buyer_input.clone(),
            buyers_warning_tx_seller_input: nonces.buyers_warning_tx_seller_input.clone(),
            sellers_warning_tx_buyer_input: nonces.sellers_warning_tx_buyer_input.clone(),
            sellers_warning_tx_seller_input: nonces.sellers_warning_tx_seller_input.clone(),
            buyers_redirect_tx_input: nonces.buyers_redirect_tx_input.clone(),
            sellers_redirect_tx_input: nonces.sellers_redirect_tx_input.clone(),
        };
        (half_deposit_psbt, addresses, nonces)
    }

    fn receive_nonce_shares(
        trader: &mut Trader,
        peers_message: SentNonceShares,
        redirection_receivers: [(&str, u64); 3],
    ) -> Result<()> {
        trader
            .model
            .receive_nonce_shares(Ok(peers_message), receivers(redirection_receivers))
    }

    fn sent_partial_signatures(trader: &Trader) -> ExchangedSigs<'static, ByVal> {
        let sigs = trader
            .model
            .my_partial_signatures_on_peer_txs()
            .expect("signatures exist");
        ExchangedSigs {
            peers_warning_tx_buyer_input_partial_signature: *sigs
                .peers_warning_tx_buyer_input_partial_signature,
            peers_warning_tx_seller_input_partial_signature: *sigs
                .peers_warning_tx_seller_input_partial_signature,
            peers_redirect_tx_input_partial_signature: *sigs
                .peers_redirect_tx_input_partial_signature,
            swap_tx_input_partial_signature: sigs.swap_tx_input_partial_signature.copied(),
            swap_tx_input_sighash: sigs.swap_tx_input_sighash.copied(),
            contractual_txids: None,
        }
    }

    fn receive_partial_signatures(trader: &mut Trader, sigs: &ExchangedSigs<ByVal>) -> Result<()> {
        let peers_partial_signatures = ExchangedSigs {
            peers_warning_tx_buyer_input_partial_signature: sigs
                .peers_warning_tx_buyer_input_partial_signature,
            peers_warning_tx_seller_input_partial_signature: sigs
                .peers_warning_tx_seller_input_partial_signature,
            peers_redirect_tx_input_partial_signature: sigs
                .peers_redirect_tx_input_partial_signature,
            swap_tx_input_partial_signature: sigs.swap_tx_input_partial_signature,
            swap_tx_input_sighash: sigs.swap_tx_input_sighash,
            contractual_txids: None,
        };
        trader
            .model
            .receive_partial_signatures(&mut *trader.wallet, Ok(peers_partial_signatures))
    }

    /// The two traders after the exchange of the key shares.
    fn traders_before_nonce_exchange((my_role, peers_role): (Role, Role)) -> (Trader, Trader) {
        let mut me = new_trader(my_role);
        let mut peer = new_trader(peers_role);
        receive_key_shares(&mut me, &sent_key_shares(&peer)).expect("valid key shares");
        receive_key_shares(&mut peer, &sent_key_shares(&me)).expect("valid key shares");
        (me, peer)
    }

    /// The two traders after the exchange of key shares and nonce shares. Both hold their partial
    /// signatures, but did not yet process the partial signatures of the peer.
    fn traders_before_deposit_signature(
        roles: (Role, Role),
        peers_redirection_receivers: [(&str, u64); 3],
        prepare_peer: impl FnOnce(&mut TradeModel),
    ) -> (Trader, Trader) {
        let mut me = new_trader(roles.0);
        let mut peer = new_trader(roles.1);
        prepare_peer(&mut peer.model);
        receive_key_shares(&mut me, &sent_key_shares(&peer)).expect("valid key shares");
        receive_key_shares(&mut peer, &sent_key_shares(&me)).expect("valid key shares");
        receive_nonce_shares(&mut me, sent_nonce_shares(&peer), REDIRECTION_RECEIVERS)
            .expect("valid nonce shares");
        receive_nonce_shares(
            &mut peer,
            sent_nonce_shares(&me),
            peers_redirection_receivers,
        )
        .expect("valid nonce shares");
        (me, peer)
    }

    /// The two traders after both signed the deposit transaction.
    pub(super) fn traders_after_deposit_signature(roles: (Role, Role)) -> (Trader, Trader) {
        let (mut me, mut peer) =
            traders_before_deposit_signature(roles, REDIRECTION_RECEIVERS, |_| {});
        let peers_partial_signatures = sent_partial_signatures(&peer);
        receive_partial_signatures(&mut peer, &sent_partial_signatures(&me))
            .expect("valid signatures");
        receive_partial_signatures(&mut me, &peers_partial_signatures).expect("valid signatures");
        (me, peer)
    }

    fn random_partial_signature() -> PartialSignature {
        Scalar::random(&mut rand::rng()).into()
    }

    /// Checks that the request terminated the trade, with a reason that contains `reason`, and
    /// that the trade did not change its state.
    #[track_caller]
    fn assert_terminated<T: std::fmt::Debug>(result: &Result<T>, trader: &Trader, reason: &str) {
        let Err(ProtocolErrorKind::TradeTerminated(actual_reason)) = result else {
            panic!("the trade must be terminated: {result:?}");
        };
        assert!(
            actual_reason.contains(reason),
            "unexpected reason: {actual_reason}"
        );
        assert_eq!(trader.model.termination_reason(), Some(&actual_reason[..]));
    }

    /// Checks that the request was refused without terminating the trade.
    #[track_caller]
    fn assert_refused<T: std::fmt::Debug>(result: &Result<T>, trader: &Trader) {
        let Err(error) = result else {
            panic!("the request must be refused: {result:?}")
        };
        assert!(error.is_refusal(), "unexpected error: {error:?}");
        assert_eq!(trader.model.termination_reason(), None);
    }

    /// Checks that the trader did not sign the deposit transaction.
    #[track_caller]
    fn assert_deposit_not_signed(trader: &Trader) {
        assert_eq!(trader.model.state(), TradeState::Init);
        assert!(matches!(
            trader.model.deposit_psbt(),
            Err(ProtocolErrorKind::AccessDenied)
        ));
        let psbt = trader
            .model
            .deposit_tx
            .builder
            .psbt()
            .expect("unsigned PSBT exists");
        assert!(
            psbt.inputs
                .iter()
                .all(|input| input.final_script_witness.is_none())
        );
    }

    /// The setup with valid data succeeds for every role. This checks the helpers of this module.
    #[test]
    fn valid_setup_signs_the_deposit() {
        for roles in ROLE_PAIRS {
            let (me, peer) = traders_after_deposit_signature(roles);
            assert_eq!(me.model.state(), TradeState::Deposit);
            assert_eq!(peer.model.state(), TradeState::Deposit);
            assert_eq!(
                me.model.deposit_psbt().expect("signed").unsigned_tx,
                peer.model.deposit_psbt().expect("signed").unsigned_tx
            );
            // D4: the redirect transactions have a relative lock time of one block.
            let redirect_tx = me.model.my_signed_redirect_tx().expect("signed");
            assert_eq!(
                redirect_tx.input[0].sequence,
                REDIRECT_LOCK_TIME.to_sequence()
            );
            // The lock times of warning and claim transaction come from the configuration.
            let warning_tx = me.model.my_signed_warning_tx().expect("signed");
            assert!(warning_tx.input.iter().all(|i| i.sequence.0 == 20));
            // D3: the claim is built when it is needed, with the fee rate of that moment, and
            // spends the escrow of the trader's own warning through the script path.
            let claim_tx = me
                .model
                .my_claim_tx(FeeRate::from_sat_per_kwu(2_500))
                .expect("built and signed");
            assert_eq!(claim_tx.input[0].sequence.0, 10);
            assert_eq!(
                claim_tx.input[0].previous_output,
                OutPoint::new(warning_tx.compute_txid(), 0)
            );
            assert_eq!(claim_tx.input[0].witness.len(), 3, "script path spend");
        }
    }

    /// The trade store names the trade that has reserved a coin, as long as the trade holds the
    /// reservation. The wallet service uses this to refuse a release of such a coin by hand.
    #[test]
    fn store_names_the_trade_that_reserved_a_coin() {
        let (me, _peer) = traders_before_nonce_exchange(ROLE_PAIRS[0]);
        let coins = me.model.reserved_coins().to_vec();
        assert!(!coins.is_empty(), "the half deposit reserves coins");
        let Trader { model, mut wallet } = me;
        let trade_id = model.trade_id().to_owned();

        let store = MemoryTradeStore::default();
        store.add_trade_model(model).expect("new trade");
        for coin in &coins {
            assert_eq!(store.trade_reserving(coin), Some(trade_id.clone()));
        }
        let other_coin = OutPoint::new(coins[0].txid, coins[0].vout + 7);
        assert_eq!(store.trade_reserving(&other_coin), None);

        // An abandoned trade has released its coins.
        let trade = store.get_trade_model(&trade_id).expect("stored");
        trade
            .lock()
            .unwrap()
            .abandon(&mut *wallet)
            .expect("in Init");
        assert_eq!(store.trade_reserving(&coins[0]), None);
    }

    /// H9: the key shares of a trade come from the wallet (D10). The same wallet seed and the same
    /// trade ID give the same key shares again, for example after a restart. Another trade ID
    /// or another wallet gives other key shares.
    #[test]
    fn h9_key_shares_are_derived_from_seed_and_trade_id() {
        let public_key_shares = |role, trade_id| {
            let trader = new_trader_for_trade(role, trade_id);
            let key_shares = trader.model.my_key_shares().expect("key shares exist");
            [*key_shares.buyer_payout, *key_shares.seller_payout]
        };
        let buyer = public_key_shares(Role::BuyerAsTaker, "trade-1");
        assert_eq!(public_key_shares(Role::BuyerAsTaker, "trade-1"), buyer);
        // The role label of the trader has no influence, only the payout has.
        assert_eq!(public_key_shares(Role::BuyerAsMaker, "trade-1"), buyer);
        assert_ne!(buyer[0], buyer[1]);

        let other_trade = public_key_shares(Role::BuyerAsTaker, "trade-2");
        let other_wallet = public_key_shares(Role::SellerAsMaker, "trade-1");
        for other in [other_trade, other_wallet] {
            for key_share in other {
                assert!(!buyer.contains(&key_share));
            }
        }
    }

    /// G1: the peer uses one key share for both payouts.
    #[test]
    fn g1_peers_key_shares_are_equal() {
        for (my_role, peers_role) in ROLE_PAIRS {
            let mut me = new_trader(my_role);
            let peers_key_shares = sent_key_shares(&new_trader(peers_role));
            let equal_key_shares = ExchangedKeys {
                seller_payout: peers_key_shares.buyer_payout,
                ..peers_key_shares
            };
            let result = receive_key_shares(&mut me, &equal_key_shares);
            assert_terminated(
                &result,
                &me,
                "key shares of the peer for the two payouts are equal",
            );
            assert_eq!(me.model.state(), TradeState::Init);
        }
    }

    /// G1: the peer sends one of our own key shares back as its key share.
    #[test]
    fn g1_peers_key_share_is_equal_to_own() {
        for (my_role, peers_role) in ROLE_PAIRS {
            for for_buyer_payout in [true, false] {
                let mut me = new_trader(my_role);
                let my_key_shares = sent_key_shares(&me);
                let peers_key_shares = sent_key_shares(&new_trader(peers_role));
                let key_shares_with_own = if for_buyer_payout {
                    ExchangedKeys {
                        buyer_payout: my_key_shares.buyer_payout,
                        ..peers_key_shares
                    }
                } else {
                    ExchangedKeys {
                        seller_payout: my_key_shares.seller_payout,
                        ..peers_key_shares
                    }
                };
                let result = receive_key_shares(&mut me, &key_shares_with_own);
                assert_terminated(&result, &me, "equals one of our own key shares");
                assert_eq!(me.model.state(), TradeState::Init);
            }
        }
    }

    /// G1: the peer sends our own script key back as its script key.
    #[test]
    fn g1_peers_script_key_is_equal_to_own() {
        for (my_role, peers_role) in ROLE_PAIRS {
            let mut me = new_trader(my_role);
            let peers_key_shares = sent_key_shares(&new_trader(peers_role));
            let with_own_script_key = ExchangedKeys {
                multisig_script: sent_key_shares(&me).multisig_script,
                ..peers_key_shares
            };
            let result = receive_key_shares(&mut me, &with_own_script_key);
            assert_terminated(
                &result,
                &me,
                "script key of the peer equals our own script key",
            );
        }
    }

    /// Checks that the nonce shares are rejected and not stored, and that the rejection terminates
    /// the trade: the valid nonce shares of the peer are refused afterwards.
    #[track_caller]
    fn assert_repeated_nonce_share(
        me: &mut Trader,
        peer: &Trader,
        change: impl FnOnce(&mut ExchangedNonces<ByVal>, &ExchangedNonces<ByVal>),
    ) {
        let (half_deposit_psbt, addresses, mut nonces) = sent_nonce_shares(peer);
        change(&mut nonces, &sent_nonce_shares(me).2);
        let result = receive_nonce_shares(
            me,
            (half_deposit_psbt, addresses, nonces),
            REDIRECTION_RECEIVERS,
        );
        assert_terminated(&result, me, "nonce share");
        assert!(
            matches!(
                me.model.aggregate_nonce_shares(),
                Err(ProtocolErrorKind::Multisig(_))
            ),
            "no nonce share of the rejected request is stored"
        );

        let result = receive_nonce_shares(me, sent_nonce_shares(peer), REDIRECTION_RECEIVERS);
        assert_terminated(&result, me, "nonce share");
        assert!(
            matches!(
                me.model.update_state(TradeState::Deposit),
                Err(ProtocolErrorKind::TradeTerminated(_))
            ),
            "a terminated trade does not change its state"
        );
        assert_eq!(me.model.state(), TradeState::Init);
    }

    /// G4: the peer uses one nonce for two signatures.
    #[test]
    fn g4_peers_nonce_is_repeated() {
        for roles in ROLE_PAIRS {
            let (mut me, peer) = traders_before_nonce_exchange(roles);
            assert_repeated_nonce_share(&mut me, &peer, |nonces, _| {
                nonces.buyers_redirect_tx_input = nonces.sellers_redirect_tx_input.clone();
            });

            // The nonce for the swap transaction belongs to an adaptor signature. It is checked
            // like all others.
            let (mut me, peer) = traders_before_nonce_exchange(roles);
            assert_repeated_nonce_share(&mut me, &peer, |nonces, _| {
                nonces.swap_tx_input = nonces.sellers_warning_tx_seller_input.clone();
            });
        }
    }

    /// The reason that is logged when a trade is terminated names both signatures and the nonce.
    #[test]
    fn g4_reason_of_termination_is_detailed() {
        let (mut me, peer) = traders_before_nonce_exchange(ROLE_PAIRS[0]);
        let nonce_shares = |second: PubNonce, last: PubNonce| {
            let nonces = sent_nonce_shares(&peer).2;
            [
                nonces.swap_tx_input,
                second,
                nonces.buyers_warning_tx_seller_input,
                nonces.sellers_warning_tx_buyer_input,
                nonces.sellers_warning_tx_seller_input,
                nonces.buyers_redirect_tx_input,
                last,
            ]
        };
        let peers = sent_nonce_shares(&peer).2;
        let own = sent_nonce_shares(&me).2.buyers_warning_tx_seller_input;
        let repeated = peers.buyers_redirect_tx_input;

        // Without a repeated nonce share there is no reason.
        let valid = nonce_shares(
            peers.buyers_warning_tx_buyer_input.clone(),
            peers.sellers_redirect_tx_input.clone(),
        );
        assert_eq!(
            me.model
                .find_repeated_nonce_share(&valid)
                .expect("nonces exist"),
            None
        );

        // One of our own nonce shares.
        let with_own = nonce_shares(own.clone(), peers.sellers_redirect_tx_input);
        assert_eq!(
            me.model
                .find_repeated_nonce_share(&with_own)
                .expect("nonces exist"),
            Some(format!(
                "the peer's nonce share for the buyer's warning transaction (buyer input) equals \
                 our own nonce share for the buyer's warning transaction (seller input) (nonce \
                 share {own})"
            ))
        );

        // A nonce share that the peer uses twice.
        let with_repeated = nonce_shares(peers.buyers_warning_tx_buyer_input, repeated.clone());
        assert_eq!(
            me.model
                .find_repeated_nonce_share(&with_repeated)
                .expect("nonces exist"),
            Some(format!(
                "the peer's nonce share for the seller's redirect transaction equals the peer's \
                 nonce share for the buyer's redirect transaction (nonce share {repeated})"
            ))
        );
    }

    /// G4: the peer sends one of our own nonces back, for the same signature or for another one.
    #[test]
    fn g4_peers_nonce_is_equal_to_own() {
        for roles in ROLE_PAIRS {
            let (mut me, peer) = traders_before_nonce_exchange(roles);
            assert_repeated_nonce_share(&mut me, &peer, |nonces, my_nonces| {
                nonces.buyers_warning_tx_buyer_input =
                    my_nonces.buyers_warning_tx_buyer_input.clone();
            });

            let (mut me, peer) = traders_before_nonce_exchange(roles);
            assert_repeated_nonce_share(&mut me, &peer, |nonces, my_nonces| {
                nonces.sellers_redirect_tx_input = my_nonces.swap_tx_input.clone();
            });
        }
    }

    /// G5: an invalid partial signature of the peer for the warning transaction, the redirect
    /// transaction or the swap transaction terminates the trade. The deposit transaction is not
    /// signed, and the valid signatures are refused afterwards (G13).
    #[test]
    fn g5_invalid_partial_signature() {
        type Change = fn(&mut ExchangedSigs<ByVal>);
        let changes: [(&str, Change); 4] = [
            ("warning transaction, buyer input", |sigs| {
                sigs.peers_warning_tx_buyer_input_partial_signature = random_partial_signature();
            }),
            ("warning transaction, seller input", |sigs| {
                sigs.peers_warning_tx_seller_input_partial_signature = random_partial_signature();
            }),
            ("redirect transaction", |sigs| {
                sigs.peers_redirect_tx_input_partial_signature = random_partial_signature();
            }),
            // Only the seller sends this signature during the setup, so only the buyer checks it.
            ("swap transaction", |sigs| {
                sigs.swap_tx_input_partial_signature = Some(random_partial_signature());
            }),
        ];
        for roles in ROLE_PAIRS {
            for (name, change) in changes {
                let (mut me, peer) =
                    traders_before_deposit_signature(roles, REDIRECTION_RECEIVERS, |_| {});
                if name == "swap transaction" && !me.model.am_buyer() {
                    continue;
                }
                let mut peers_partial_signatures = sent_partial_signatures(&peer);
                change(&mut peers_partial_signatures);

                let result = receive_partial_signatures(&mut me, &peers_partial_signatures);
                assert_terminated(&result, &me, "signature is invalid");
                assert_deposit_not_signed(&me);

                let result = receive_partial_signatures(&mut me, &sent_partial_signatures(&peer));
                assert_terminated(&result, &me, "signature is invalid");
                assert_deposit_not_signed(&me);
            }
        }
    }

    /// G5: the seller must send the signature hash and his partial signature for the swap
    /// transaction with his partial signatures.
    #[test]
    fn g5_missing_swap_tx_data_of_the_seller() {
        for roles in [ROLE_PAIRS[0], ROLE_PAIRS[1]] {
            let (mut me, peer) =
                traders_before_deposit_signature(roles, REDIRECTION_RECEIVERS, |_| {});
            let mut peers_partial_signatures = sent_partial_signatures(&peer);
            peers_partial_signatures.swap_tx_input_sighash = None;
            let result = receive_partial_signatures(&mut me, &peers_partial_signatures);
            assert_terminated(&result, &me, "no signature hash or no partial signature");
            assert_deposit_not_signed(&me);
        }
    }

    /// G6: the peer signed transactions with other outputs. The peer computed its redirect
    /// transactions with other receiver amounts, so its partial signature is for another message.
    #[test]
    fn g6_partial_signature_for_other_outputs() {
        for roles in ROLE_PAIRS {
            let (mut me, peer) =
                traders_before_deposit_signature(roles, OTHER_REDIRECTION_RECEIVERS, |_| {});

            let result = receive_partial_signatures(&mut me, &sent_partial_signatures(&peer));
            // The reason names both possible causes (D17).
            assert_terminated(
                &result,
                &me,
                "the two traders use different trade parameters",
            );
            assert_deposit_not_signed(&me);
        }
    }

    /// G6: the engine of the peer is configured with another warning lock time. The peer computed
    /// its warning transactions with this lock time, so its partial signatures are for other
    /// messages.
    #[test]
    fn g6_partial_signature_for_other_lock_time() {
        for roles in ROLE_PAIRS {
            let mut me = new_trader(roles.0);
            let mut peer = new_trader(roles.1);
            receive_key_shares(&mut me, &sent_key_shares(&peer)).expect("valid key shares");
            let peers_config = EngineConfig::new(21, 10, 144, 6).expect("valid configuration");
            peer.model
                .receive_key_shares(
                    &mut *peer.wallet,
                    &peers_config,
                    Ok(sent_key_shares(&me)),
                    params(),
                )
                .expect("valid key shares");
            receive_nonce_shares(&mut me, sent_nonce_shares(&peer), REDIRECTION_RECEIVERS)
                .expect("valid nonce shares");
            receive_nonce_shares(&mut peer, sent_nonce_shares(&me), REDIRECTION_RECEIVERS)
                .expect("valid nonce shares");

            let result = receive_partial_signatures(&mut me, &sent_partial_signatures(&peer));
            // The reason names the own lock times, so that the user can compare them.
            assert_terminated(
                &result,
                &me,
                "different lock times; our configuration: warning lock time 20 blocks, claim lock \
                 time 10 blocks",
            );
            assert_deposit_not_signed(&me);
        }
    }

    /// G14 (D18): a request that is refused leaves no data in the trade. Invalid trade parameters
    /// and wrong redirection receivers of the caller are refused, and the corrected request
    /// succeeds afterwards (G15).
    #[test]
    fn g14_refused_request_leaves_no_data() {
        for (my_role, peers_role) in ROLE_PAIRS {
            let mut me = new_trader(my_role);
            let mut peer = new_trader(peers_role);

            // Requests in the wrong order.
            let result = receive_nonce_shares(
                &mut me,
                sent_nonce_shares_placeholder(),
                REDIRECTION_RECEIVERS,
            );
            assert_refused(&result, &me);

            // Invalid trade parameters of the caller.
            let invalid_params = TradeParams {
                sellers_security_deposit: Amount::from_sat(100),
                ..params()
            };
            let result = me.model.receive_key_shares(
                &mut *me.wallet,
                &config(),
                Ok(sent_key_shares(&peer)),
                invalid_params,
            );
            assert_refused(&result, &me);
            assert!(
                me.model.my_half_deposit_psbt().is_err(),
                "nothing is stored"
            );

            receive_key_shares(&mut me, &sent_key_shares(&peer)).expect("valid key shares");
            receive_key_shares(&mut peer, &sent_key_shares(&me)).expect("valid key shares");

            // The same request again returns the same result. Other parameters are refused.
            receive_key_shares(&mut me, &sent_key_shares(&peer)).expect("repeated request");
            let other_params = TradeParams {
                trade_amount: Amount::from_sat(100_000),
                ..params()
            };
            let result = me.model.receive_key_shares(
                &mut *me.wallet,
                &config(),
                Ok(sent_key_shares(&peer)),
                other_params,
            );
            assert_refused(&result, &me);

            // Wrong redirection receivers of the caller: 10,000 sat are missing.
            let mut wrong_receivers = REDIRECTION_RECEIVERS;
            wrong_receivers[0].1 -= 10_000;
            let result = receive_nonce_shares(&mut me, sent_nonce_shares(&peer), wrong_receivers);
            assert_refused(&result, &me);
            assert!(
                me.model.my_partial_signatures_on_peer_txs().is_err(),
                "nothing is signed"
            );

            receive_nonce_shares(&mut me, sent_nonce_shares(&peer), REDIRECTION_RECEIVERS)
                .expect("valid nonce shares after the refused ones");
            receive_nonce_shares(&mut peer, sent_nonce_shares(&me), REDIRECTION_RECEIVERS)
                .expect("valid nonce shares");
            receive_partial_signatures(&mut me, &sent_partial_signatures(&peer))
                .expect("valid signatures");
            assert_eq!(me.model.state(), TradeState::Deposit);
        }
    }

    /// Nonce shares of a trader with another trade, for a request that must be refused before
    /// its data is looked at.
    fn sent_nonce_shares_placeholder() -> SentNonceShares {
        let (_, peer) = traders_before_nonce_exchange(ROLE_PAIRS[0]);
        sent_nonce_shares(&peer)
    }

    /// G12: data of the peer that differs from the data that it sent before terminates the trade.
    #[test]
    fn g12_second_version_of_peer_data() {
        for roles in ROLE_PAIRS {
            let (mut me, _peer) = traders_after_deposit_signature(roles);
            let other_peer = new_trader_for_trade(roles.1, "other trade");
            let result = receive_key_shares(&mut me, &sent_key_shares(&other_peer));
            assert_terminated(&result, &me, "differ from the ones that it sent before");
            assert_eq!(me.model.state(), TradeState::Deposit);

            let (mut me, peer) = traders_after_deposit_signature(roles);
            let mut other_signatures = sent_partial_signatures(&peer);
            other_signatures.peers_redirect_tx_input_partial_signature = random_partial_signature();
            let result = receive_partial_signatures(&mut me, &other_signatures);
            assert_terminated(&result, &me, "differ from the ones that it sent before");
        }
    }

    /// G13 and D18: data of the peer that cannot be decoded terminates the trade only if the
    /// request is allowed. The engine checks the preconditions of the request first.
    #[test]
    fn g13_undecodable_peer_data_is_checked_after_the_preconditions() {
        fn undecodable<T>() -> PeerData<T> {
            Err(UndecodablePeerData("invalid length".to_owned()))
        }
        for roles in ROLE_PAIRS {
            // None of these requests is allowed before the key shares of the peer arrived.
            let mut me = new_trader(roles.0);
            let receivers = receivers(REDIRECTION_RECEIVERS);
            let result = me.model.receive_nonce_shares(undecodable(), receivers);
            assert_refused(&result, &me);
            let result = me
                .model
                .receive_partial_signatures(&mut *me.wallet, undecodable());
            assert_refused(&result, &me);
            let result = me
                .model
                .receive_deposit_psbt(&mut *me.wallet, undecodable());
            assert_refused(&result, &me);
            let result = me.model.receive_swap_tx_signature(undecodable());
            assert_refused(&result, &me);
            let result = me
                .model
                .close_with_key_share(&mut *me.wallet, undecodable(), None);
            assert_refused(&result, &me);
            let result = me
                .model
                .finalize_custom_payout(&mut *me.wallet, undecodable());
            assert_refused(&result, &me);
            assert_eq!(me.model.state(), TradeState::Init);

            // The key shares of the peer are allowed now.
            let result =
                me.model
                    .receive_key_shares(&mut *me.wallet, &config(), undecodable(), params());
            assert_terminated(&result, &me, "cannot be decoded: invalid length");
            assert!(
                me.model.my_half_deposit_psbt().is_err(),
                "nothing is stored"
            );

            // A repeated request with data that cannot be decoded differs from the first one.
            let (mut me, _peer) = traders_after_deposit_signature(roles);
            let result = me
                .model
                .receive_partial_signatures(&mut *me.wallet, undecodable());
            assert_terminated(&result, &me, "cannot be decoded: invalid length");
            assert_eq!(me.model.state(), TradeState::Deposit);
        }
    }

    /// F4: `close_trade` returns an error for the closure types that are not implemented. It
    /// does not panic and does not change the state.
    #[test]
    fn f4_close_trade_with_dispute_closure_type() {
        for roles in ROLE_PAIRS {
            let (mut me, _peer) = traders_after_deposit_signature(roles);
            for closure_type in [
                ClosureType::BuyersRedirect,
                ClosureType::SellersRedirect,
                ClosureType::BuyersClaim,
                ClosureType::SellersClaim,
                ClosureType::BuyersPenaltyTx,
                ClosureType::SellersPenaltyTx,
            ] {
                assert!(
                    matches!(
                        me.model.close_trade(closure_type),
                        Err(ProtocolErrorKind::UnsupportedClosureType(c)) if c == closure_type
                    ),
                    "{closure_type:?}"
                );
                assert_eq!(me.model.state(), TradeState::Deposit);
            }
        }
    }
}

#[cfg(test)]
#[path = "trade_model_chain_tests.rs"]
mod chain_tests;
