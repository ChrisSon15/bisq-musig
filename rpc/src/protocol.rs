use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::sync::{Arc, LazyLock, Mutex};

use bdk_wallet::bitcoin::address::{NetworkChecked, NetworkUnchecked, NetworkValidation};
use bdk_wallet::bitcoin::{
    Address, Amount, FeeRate, Network, Psbt, TapSighash, Transaction, Txid, XOnlyPublicKey,
};
use guardian::ArcMutexGuardian;
use musig2::secp::{MaybeScalar, Point, Scalar};
use musig2::{PartialSignature, PubNonce};
use protocol::multisig::{KeyCtx, KeyPair, PointExt as _, SigCtx};
use protocol::receiver::{Receiver, ReceiverList};
use protocol::state::{ClosureType, TradeState};
use protocol::transaction::{
    ClaimTxBuilder, CustomPayoutTxBuilder, DepositTxBuilder, NetworkParams as _, PenaltyTxBuilder,
    RedirectTxBuilder, SwapTxBuilder, TransactionExt as _, WarningTxBuilder,
};
use protocol::{mocks, script_paths};
use thiserror::Error;
use tracing::{info, instrument};
use wallet::protocol_wallet_api::ProtocolWalletApi;

use crate::storage::{ByRef, ByVal, Storage};

pub trait TradeModelStore {
    /// Stores a new trade model. A trade ID identifies one trade for the whole lifetime of the
    /// store, so a trade model with the ID of a stored trade is rejected, also if that trade is
    /// closed. The stored trade model is not changed.
    fn add_trade_model(&self, trade_model: TradeModel) -> Result<()>;
    fn get_trade_model(&self, trade_id: &str) -> Option<Arc<Mutex<TradeModel>>>;
}

type TradeModelMemoryStore = Mutex<BTreeMap<String, Arc<Mutex<TradeModel>>>>;

impl TradeModelStore for TradeModelMemoryStore {
    fn add_trade_model(&self, trade_model: TradeModel) -> Result<()> {
        match self.lock().unwrap().entry(trade_model.trade_id.clone()) {
            Entry::Occupied(entry) => Err(ProtocolErrorKind::DuplicateTradeId(entry.key().clone())),
            Entry::Vacant(entry) => {
                entry.insert(Arc::new(Mutex::new(trade_model)));
                Ok(())
            }
        }
    }

    fn get_trade_model(&self, trade_id: &str) -> Option<Arc<Mutex<TradeModel>>> {
        self.lock().unwrap().get(trade_id).map(Arc::clone)
    }
}

pub static TRADE_MODELS: LazyLock<TradeModelMemoryStore> = LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[derive(Default)]
pub struct TradeModel {
    trade_id: String,
    my_role: Role,
    state: TradeState,
    trade_wallet: Option<Arc<Mutex<dyn ProtocolWalletApi + Send + 'static>>>,
    keys: Keys,
    deposit_tx: DepositTx,
    swap_tx: SwapTx,
    penalty_tx: PenaltyTx,
    custom_payout_tx: CustomPayoutTx,
    buyer_txs: ArbitrationTxs,
    seller_txs: ArbitrationTxs,
}

#[derive(Copy, Clone, Eq, PartialEq, Default, Debug)]
pub enum Role {
    #[default] SellerAsMaker,
    SellerAsTaker,
    BuyerAsMaker,
    BuyerAsTaker,
}

#[derive(Default)]
struct Keys {
    am_buyer: bool,
    buyer_payout_ctx: KeyCtx,
    seller_payout_ctx: KeyCtx,
    my_multisig_script_key: Option<XOnlyPublicKey>,
    peers_multisig_script_key: Option<XOnlyPublicKey>,
}

#[derive(Default)]
struct ArbitrationTxs {
    warning: WarningTx,
    redirect: RedirectTx,
    claim: ClaimTx,
}

#[derive(Default)]
struct DepositTx {
    builder: DepositTxBuilder,
}

#[derive(Default)]
struct SwapTx {
    builder: SwapTxBuilder,
    input_sighash: Option<TapSighash>,
    input_sig_ctx: SigCtx,
}

#[derive(Default)]
struct WarningTx {
    builder: WarningTxBuilder,
    buyer_input_sig_ctx: SigCtx,
    seller_input_sig_ctx: SigCtx,
}

#[derive(Default)]
struct RedirectTx {
    builder: RedirectTxBuilder,
    input_sig_ctx: SigCtx,
}

#[derive(Default)]
struct ClaimTx {
    builder: ClaimTxBuilder,
    input_sig_ctx: SigCtx,
}

#[derive(Default)]
struct PenaltyTx {
    builder: PenaltyTxBuilder,
    input_sig_ctx: SigCtx,
}

#[derive(Default)]
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

impl<'a> ExchangedAddresses<'a, ByVal, NetworkUnchecked> {
    fn require_network(self, required: Network) -> Result<ExchangedAddresses<'a, ByVal>> {
        Ok(ExchangedAddresses {
            warning_tx_fee_bump: self.warning_tx_fee_bump.require_network(required)?,
            redirect_tx_fee_bump: self.redirect_tx_fee_bump.require_network(required)?,
            claim_tx_payout: self.claim_tx_payout.require_network(required)?,
        })
    }
}

#[expect(clippy::struct_field_names,
reason = "removing common suffix probably wouldn't make things clearer")]
pub struct ExchangedNonces<'a, S: Storage> {
    pub swap_tx_input: S::Store<'a, PubNonce>,
    pub buyers_warning_tx_buyer_input: S::Store<'a, PubNonce>,
    pub buyers_warning_tx_seller_input: S::Store<'a, PubNonce>,
    pub sellers_warning_tx_buyer_input: S::Store<'a, PubNonce>,
    pub sellers_warning_tx_seller_input: S::Store<'a, PubNonce>,
    pub buyers_redirect_tx_input: S::Store<'a, PubNonce>,
    pub sellers_redirect_tx_input: S::Store<'a, PubNonce>,
    pub buyers_claim_tx_input: S::Store<'a, PubNonce>,
    pub sellers_claim_tx_input: S::Store<'a, PubNonce>,
}

pub struct ExchangedSigs<'a, S: Storage> {
    pub peers_warning_tx_buyer_input_partial_signature: S::Store<'a, PartialSignature>,
    pub peers_warning_tx_seller_input_partial_signature: S::Store<'a, PartialSignature>,
    pub peers_redirect_tx_input_partial_signature: S::Store<'a, PartialSignature>,
    pub peers_claim_tx_input_partial_signature: S::Store<'a, PartialSignature>,
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

macro_rules! access {
    ($name:ident, $pattern:pat $(if $guard:expr)?) => {
        {
            use ClosureType::*;
            use TradeState::*;
            #[expect(path_statements, reason="HACK to prevent `unused_imports` warning")]
            Cooperative;
            match $name.state {
                $pattern $(if $guard)? => Ok(()),
                _ => Err(ProtocolErrorKind::AccessDenied)
            }
        }
    };
}

impl TradeModel {
    pub fn new(trade_id: String, my_role: Role) -> Self {
        let mut trade_model = Self { trade_id, my_role, ..Default::default() };
        let network = trade_model.trade_wallet.insert(if trade_model.am_buyer() {
            Arc::new(Mutex::new(mocks::mock_buyer_trade_wallet()))
        } else {
            Arc::new(Mutex::new(mocks::mock_seller_trade_wallet()))
        }).lock().unwrap().network();
        for txs in [&mut trade_model.buyer_txs, &mut trade_model.seller_txs] {
            txs.warning.builder.set_lock_time(network.warning_lock_time());
            txs.redirect.builder.set_lock_time(network.redirect_lock_time());
            txs.claim.builder.set_lock_time(network.claim_lock_time());
        }
        trade_model.swap_tx.builder.disable_lock_time();
        trade_model.penalty_tx.builder.disable_lock_time();
        trade_model.keys.am_buyer = trade_model.am_buyer();
        trade_model
    }

    pub const fn am_buyer(&self) -> bool {
        matches!(self.my_role, Role::BuyerAsMaker | Role::BuyerAsTaker)
    }

    pub const fn state(&self) -> TradeState {
        self.state
    }

    fn trade_wallet(&self) -> Result<ArcMutexGuardian<dyn ProtocolWalletApi + Send + 'static>> {
        Ok(ArcMutexGuardian::take(self.trade_wallet.clone()
            .ok_or(ProtocolErrorKind::MissingTradeWallet)?).unwrap())
    }

    #[instrument(skip_all)]
    fn update_state(&mut self, new_state: TradeState) -> Result<bool> {
        if self.state >= new_state {
            return Ok(false);
        }
        let (role, old_state) = (self.my_role, self.state);
        if self.am_buyer() && !self.state.precedes_for_buyer(new_state) ||
            !self.am_buyer() && !self.state.precedes_for_seller(new_state) {
            return Err(ProtocolErrorKind::IllegalStateTransition { role, old_state, new_state });
        }
        // FIXME: Persist the trade model together with its updated state. Assuming asynchronous
        //  persistence, we must ensure that the old state continues to be observed meanwhile.
        info!(trade_id = self.trade_id, %old_state, %new_state, "Updating trade state.");
        self.state = new_state;
        Ok(true)
    }

    pub fn set_trade_amount(&mut self, trade_amount: Amount) {
        self.deposit_tx.builder.set_trade_amount(trade_amount);
    }

    pub fn set_buyers_security_deposit(&mut self, buyers_security_deposit: Amount) {
        self.deposit_tx.builder.set_buyers_security_deposit(buyers_security_deposit);
    }

    pub fn set_sellers_security_deposit(&mut self, sellers_security_deposit: Amount) {
        self.deposit_tx.builder.set_sellers_security_deposit(sellers_security_deposit);
    }

    pub fn set_deposit_tx_fee_rate(&mut self, fee_rate: FeeRate) {
        self.deposit_tx.builder.set_fee_rate(fee_rate);
    }

    fn prepared_tx_fee_rate(&self) -> Result<FeeRate> { Ok(*self.swap_tx.builder.fee_rate()?) }

    pub fn set_prepared_tx_fee_rate(&mut self, fee_rate: FeeRate) {
        for txs in [&mut self.buyer_txs, &mut self.seller_txs] {
            txs.warning.builder.set_fee_rate(fee_rate);
            txs.claim.builder.set_fee_rate(fee_rate);
        }
        self.swap_tx.builder.set_fee_rate(fee_rate);
    }

    pub fn set_penalty_tx_fee_rate(&mut self, fee_rate: Option<FeeRate>) -> Result<()> {
        self.penalty_tx.builder.set_fee_rate(fee_rate.unwrap_or(self.prepared_tx_fee_rate()?));
        Ok(())
    }

    pub fn set_trade_fee_receiver(&mut self, receiver: Option<Receiver<NetworkUnchecked>>) -> Result<()> {
        let network = self.trade_wallet()?.network();
        self.deposit_tx.builder.set_trade_fee_receivers(receiver
            .map(|r| r.require_network(network)).transpose()?
            .into_iter().collect());
        Ok(())
    }

    pub fn redirection_amount_msat(&self) -> Result<u64> {
        let split_input_amounts = [
            *self.deposit_tx.builder.trade_amount()?,
            *self.deposit_tx.builder.buyers_security_deposit()?,
            *self.deposit_tx.builder.sellers_security_deposit()?,
        ];
        let escrow_amount =
            WarningTxBuilder::escrow_amount(split_input_amounts, self.prepared_tx_fee_rate()?)?;

        Ok(RedirectTxBuilder::available_amount_msat(escrow_amount, self.prepared_tx_fee_rate()?)?)
    }

    pub fn init_my_key_shares(&mut self) -> Result<()> {
        self.keys.buyer_payout_ctx.init_my_key_share();
        self.keys.seller_payout_ctx.init_my_key_share();
        if self.keys.my_multisig_script_key.is_none() {
            self.keys.my_multisig_script_key = Some(self.trade_wallet()?.new_internal_key()?);
        }
        Ok(())
    }

    pub fn my_key_shares(&self) -> Result<ExchangedKeys<'_, ByRef>> {
        Ok(ExchangedKeys {
            buyer_payout: self.keys.buyer_payout_ctx.my_key_share()?.pub_key(),
            seller_payout: self.keys.seller_payout_ctx.my_key_share()?.pub_key(),
            multisig_script: self.keys.my_multisig_script_key.as_ref()
                .ok_or(ProtocolErrorKind::MissingScriptKey)?,
        })
    }

    pub fn set_peer_key_shares(&mut self, keys: &ExchangedKeys<ByVal>) {
        self.keys.buyer_payout_ctx.set_peers_pub_key(keys.buyer_payout);
        self.keys.seller_payout_ctx.set_peers_pub_key(keys.seller_payout);
        self.keys.peers_multisig_script_key.get_or_insert(keys.multisig_script);
    }

    // TODO: Try to refactor this method:
    pub fn aggregate_key_shares(&mut self) -> Result<()> {
        let network = self.trade_wallet()?.network();
        self.keys.buyer_payout_ctx.aggregate_pub_key_shares()?;
        self.keys.seller_payout_ctx.aggregate_pub_key_shares()?;

        let [buyer_pub_key, seller_pub_key] = self.keys.multisig_script_keys()?;
        let [buyer_internal_key, seller_internal_key] = self.keys.internal_keys()?;

        let deposit_merkle_root = script_paths::deposit_payout_merkle_root(buyer_pub_key, seller_pub_key)?;
        let buyer_payout_tweaked_key_ctx = self.keys.buyer_payout_ctx.with_taproot_tweak(
            Some(&deposit_merkle_root))?;
        let seller_payout_tweaked_key_ctx = self.keys.seller_payout_ctx.with_taproot_tweak(
            Some(&deposit_merkle_root))?;

        self.deposit_tx.builder
            .set_buyer_payout_address(buyer_payout_tweaked_key_ctx.p2tr_address(network))
            .set_seller_payout_address(seller_payout_tweaked_key_ctx.p2tr_address(network));
        self.custom_payout_tx.builder
            .set_buyer_input_descriptor(script_paths::deposit_payout_descriptor(
                &buyer_internal_key, buyer_pub_key, seller_pub_key)?)
            .set_seller_input_descriptor(script_paths::deposit_payout_descriptor(
                &seller_internal_key, buyer_pub_key, seller_pub_key)?);

        self.buyer_txs.warning.buyer_input_sig_ctx.set_tweaked_key_ctx(buyer_payout_tweaked_key_ctx.clone());
        self.seller_txs.warning.buyer_input_sig_ctx.set_tweaked_key_ctx(buyer_payout_tweaked_key_ctx);
        self.swap_tx.input_sig_ctx.set_adaptor_point(*self.keys.adaptor_key_share()?.pub_key())?;
        self.swap_tx.input_sig_ctx.set_tweaked_key_ctx(seller_payout_tweaked_key_ctx.clone());
        self.buyer_txs.warning.seller_input_sig_ctx.set_tweaked_key_ctx(seller_payout_tweaked_key_ctx.clone());
        self.seller_txs.warning.seller_input_sig_ctx.set_tweaked_key_ctx(seller_payout_tweaked_key_ctx);

        let [buyer_claim_merkle_root, seller_claim_merkle_root] = self.keys.claim_key_shares()?
            .map(|p| script_paths::warning_escrow_merkle_root(&p.pub_key().to_public_key().into(), network));
        let buyers_warning_escrow_tweaked_key_ctx = self.keys.seller_payout_ctx.with_taproot_tweak(
            Some(&buyer_claim_merkle_root?))?;
        let sellers_warning_escrow_tweaked_key_ctx = self.keys.buyer_payout_ctx.with_taproot_tweak(
            Some(&seller_claim_merkle_root?))?;

        let buyers_warning_escrow_address = buyers_warning_escrow_tweaked_key_ctx.p2tr_address(network);
        let sellers_warning_escrow_address = sellers_warning_escrow_tweaked_key_ctx.p2tr_address(network);
        self.buyer_txs.warning.builder.set_escrow_address(buyers_warning_escrow_address);
        self.seller_txs.warning.builder.set_escrow_address(sellers_warning_escrow_address);

        self.penalty_tx.input_sig_ctx.set_tweaked_key_ctx(if self.am_buyer() {
            &sellers_warning_escrow_tweaked_key_ctx
        } else {
            &buyers_warning_escrow_tweaked_key_ctx
        }.clone());

        self.buyer_txs.claim.input_sig_ctx.set_tweaked_key_ctx(buyers_warning_escrow_tweaked_key_ctx.clone());
        self.seller_txs.redirect.input_sig_ctx.set_tweaked_key_ctx(buyers_warning_escrow_tweaked_key_ctx);
        self.seller_txs.claim.input_sig_ctx.set_tweaked_key_ctx(sellers_warning_escrow_tweaked_key_ctx.clone());
        self.buyer_txs.redirect.input_sig_ctx.set_tweaked_key_ctx(sellers_warning_escrow_tweaked_key_ctx);
        Ok(())
    }

    pub fn init_my_addresses(&mut self) -> Result<()> {
        if self.penalty_tx.builder.payout_address().is_err() {
            let mut wallet = self.trade_wallet()?;
            if !self.am_buyer() {
                self.swap_tx.builder.set_payout_address(wallet.new_address()?);
            }
            let my_txs = if self.am_buyer() { &mut self.buyer_txs } else { &mut self.seller_txs };
            my_txs.warning.builder.set_anchor_address(wallet.new_address()?);
            my_txs.redirect.builder.set_anchor_address(wallet.new_address()?);
            my_txs.claim.builder.set_payout_address(wallet.new_address()?);
            self.penalty_tx.builder.set_payout_address(my_txs.claim.builder.payout_address()?.clone());
            drop(wallet);
        }
        Ok(())
    }

    pub fn my_addresses(&self) -> Result<ExchangedAddresses<'_, ByRef>> {
        let my_txs = if self.am_buyer() { &self.buyer_txs } else { &self.seller_txs };
        Ok(ExchangedAddresses {
            warning_tx_fee_bump: my_txs.warning.builder.anchor_address()?,
            redirect_tx_fee_bump: my_txs.redirect.builder.anchor_address()?,
            claim_tx_payout: my_txs.claim.builder.payout_address()?,
        })
    }

    pub fn set_peer_addresses(&mut self, addresses: ExchangedAddresses<ByVal, NetworkUnchecked>) -> Result<()> {
        let addresses = addresses.require_network(self.trade_wallet()?.network())?;
        let peer_txs = if self.am_buyer() { &mut self.seller_txs } else { &mut self.buyer_txs };
        peer_txs.warning.builder.set_anchor_address(addresses.warning_tx_fee_bump);
        peer_txs.redirect.builder.set_anchor_address(addresses.redirect_tx_fee_bump);
        peer_txs.claim.builder.set_payout_address(addresses.claim_tx_payout);
        Ok(())
    }

    pub fn init_my_half_deposit_psbt(&mut self) -> Result<()> {
        if self.my_half_deposit_psbt().is_err() {
            if self.am_buyer() {
                self.deposit_tx.builder.init_buyers_half_psbt(&mut *self.trade_wallet()?, &mut rand::rng())?;
            } else {
                self.deposit_tx.builder.init_sellers_half_psbt(&mut *self.trade_wallet()?, &mut rand::rng())?;
            }
        }
        Ok(())
    }

    pub fn my_half_deposit_psbt(&self) -> Result<&Psbt> {
        Ok(if self.am_buyer() {
            self.deposit_tx.builder.buyers_half_psbt()?
        } else {
            self.deposit_tx.builder.sellers_half_psbt()?
        })
    }

    pub fn set_peer_half_deposit_psbt(&mut self, half_deposit_psbt: Psbt) {
        if self.am_buyer() {
            self.deposit_tx.builder.set_sellers_half_psbt(half_deposit_psbt);
        } else {
            self.deposit_tx.builder.set_buyers_half_psbt(half_deposit_psbt);
        }
    }

    pub fn compute_unsigned_deposit_tx(&mut self) -> Result<()> {
        self.deposit_tx.builder.compute_unsigned_tx()?;
        Ok(())
    }

    pub fn compute_unsigned_prepared_txs(&mut self) -> Result<()> {
        if !self.am_buyer() {
            // Only the seller has all the params necessary to compute the unsigned swap tx.
            self.swap_tx.builder.set_input(self.deposit_tx.builder.seller_payout()?.clone());
            self.swap_tx.builder.compute_unsigned_tx()?;
        }
        self.buyer_txs.compute_unsigned_warning_tx(&self.deposit_tx)?;
        self.seller_txs.compute_unsigned_warning_tx(&self.deposit_tx)?;
        self.buyer_txs.compute_unsigned_redirect_and_claim_txs(&self.seller_txs)?;
        self.seller_txs.compute_unsigned_redirect_and_claim_txs(&self.buyer_txs)?;
        Ok(())
    }

    pub fn compute_unsigned_penalty_tx(&mut self) -> Result<()> {
        let peer_txs = if self.am_buyer() { &self.seller_txs } else { &self.buyer_txs };
        self.penalty_tx.builder
            .set_input(peer_txs.warning.builder.escrow()?.clone())
            .compute_unsigned_tx()?;
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

    fn redirection_receivers(&self) -> Result<&[Receiver]> {
        Ok(&self.buyer_txs.redirect.builder.receivers()?[..])
    }

    pub fn set_redirection_receivers<I, E>(&mut self, receivers: I) -> Result<(), E>
        where I: IntoIterator<Item = Result<Receiver<NetworkUnchecked>, E>>,
              E: From<ProtocolErrorKind>
    {
        let network = self.trade_wallet()?.network();
        let mut vec = Vec::new();
        for receiver in receivers {
            vec.push(receiver?.require_network(network).map_err(ProtocolErrorKind::from)?);
        }
        let receivers: ReceiverList = vec.into();
        self.buyer_txs.redirect.builder.set_receivers(receivers.clone());
        self.seller_txs.redirect.builder.set_receivers(receivers);
        Ok(())
    }

    pub fn check_redirect_tx_params(&self) -> Result<()> {
        let receivers = self.redirection_receivers()?;
        let fee_rate = self.prepared_tx_fee_rate()?;
        let available_msat = self.redirection_amount_msat()?;
        let used_msat = Receiver::total_output_cost_msat(receivers, fee_rate, 1)?;

        if used_msat > available_msat {
            return Err(ProtocolErrorKind::InsufficientRedirectionFunds { available_msat, used_msat });
        }
        if used_msat.saturating_add(999) < available_msat {
            return Err(ProtocolErrorKind::ExcessRedirectionFunds { available_msat, used_msat });
        }
        Ok(())
    }

    pub fn init_my_nonce_shares(&mut self) -> Result<()> {
        for ctx in self.all_sig_ctxs_mut() {
            ctx.init_my_nonce_share()?;
        }
        Ok(())
    }

    pub fn my_nonce_shares(&self) -> Result<ExchangedNonces<'_, ByRef>> {
        Ok(ExchangedNonces {
            swap_tx_input:
            self.swap_tx.input_sig_ctx.my_nonce_share()?,
            buyers_warning_tx_buyer_input:
            self.buyer_txs.warning.buyer_input_sig_ctx.my_nonce_share()?,
            buyers_warning_tx_seller_input:
            self.buyer_txs.warning.seller_input_sig_ctx.my_nonce_share()?,
            sellers_warning_tx_buyer_input:
            self.seller_txs.warning.buyer_input_sig_ctx.my_nonce_share()?,
            sellers_warning_tx_seller_input:
            self.seller_txs.warning.seller_input_sig_ctx.my_nonce_share()?,
            buyers_redirect_tx_input:
            self.buyer_txs.redirect.input_sig_ctx.my_nonce_share()?,
            sellers_redirect_tx_input:
            self.seller_txs.redirect.input_sig_ctx.my_nonce_share()?,
            buyers_claim_tx_input:
            self.buyer_txs.claim.input_sig_ctx.my_nonce_share()?,
            sellers_claim_tx_input:
            self.seller_txs.claim.input_sig_ctx.my_nonce_share()?,
        })
    }

    const fn all_sig_ctxs_mut(&mut self) -> [&mut SigCtx; 9] {
        [
            &mut self.swap_tx.input_sig_ctx,
            &mut self.buyer_txs.warning.buyer_input_sig_ctx,
            &mut self.buyer_txs.warning.seller_input_sig_ctx,
            &mut self.seller_txs.warning.buyer_input_sig_ctx,
            &mut self.seller_txs.warning.seller_input_sig_ctx,
            &mut self.buyer_txs.redirect.input_sig_ctx,
            &mut self.seller_txs.redirect.input_sig_ctx,
            &mut self.buyer_txs.claim.input_sig_ctx,
            &mut self.seller_txs.claim.input_sig_ctx
        ]
    }

    pub fn set_peer_nonce_shares(&mut self, nonce_shares: ExchangedNonces<ByVal>) {
        let sig_ctxs = self.all_sig_ctxs_mut();
        sig_ctxs[0].set_peers_nonce_share(nonce_shares.swap_tx_input);
        sig_ctxs[1].set_peers_nonce_share(nonce_shares.buyers_warning_tx_buyer_input);
        sig_ctxs[2].set_peers_nonce_share(nonce_shares.buyers_warning_tx_seller_input);
        sig_ctxs[3].set_peers_nonce_share(nonce_shares.sellers_warning_tx_buyer_input);
        sig_ctxs[4].set_peers_nonce_share(nonce_shares.sellers_warning_tx_seller_input);
        sig_ctxs[5].set_peers_nonce_share(nonce_shares.buyers_redirect_tx_input);
        sig_ctxs[6].set_peers_nonce_share(nonce_shares.sellers_redirect_tx_input);
        sig_ctxs[7].set_peers_nonce_share(nonce_shares.buyers_claim_tx_input);
        sig_ctxs[8].set_peers_nonce_share(nonce_shares.sellers_claim_tx_input);
    }

    pub fn aggregate_nonce_shares(&mut self) -> Result<()> {
        for ctx in self.all_sig_ctxs_mut() {
            ctx.aggregate_nonce_shares()?;
        }
        Ok(())
    }

    pub fn sign_partial(&mut self) -> Result<()> {
        for txs in [&mut self.buyer_txs, &mut self.seller_txs] {
            txs.warning.buyer_input_sig_ctx
                .sign_partial(txs.warning.builder.buyer_input_sighash()?)?;
            txs.warning.seller_input_sig_ctx
                .sign_partial(txs.warning.builder.seller_input_sighash()?)?;
            txs.redirect.input_sig_ctx
                .sign_partial(txs.redirect.builder.input_sighash()?)?;
            txs.claim.input_sig_ctx
                .sign_partial(txs.claim.builder.input_sighash()?)?;
        }
        if !self.am_buyer() {
            // Unlike the other multisig sighashes, only the seller is able to independently compute
            // the swap-tx-input sighash. The buyer must wait for the next round, when the deposit
            // tx is signed, to partially sign the swap tx using the sighash passed by the seller.
            self.sign_swap_tx_input_partial(self.swap_tx.builder.input_sighash()?)?;
        }
        Ok(())
    }

    pub fn sign_swap_tx_input_partial(&mut self, sighash: TapSighash) -> Result<()> {
        let sighash = self.swap_tx.input_sighash.get_or_insert(sighash);
        self.swap_tx.input_sig_ctx.sign_partial(*sighash)?;
        Ok(())
    }

    pub fn my_partial_signatures_on_peer_txs(&self) -> Result<ExchangedSigs<'_, ByRef>> {
        let peer_txs = if self.am_buyer() { &self.seller_txs } else { &self.buyer_txs };
        let ready_to_release = self.state == TradeState::BuyerReadyToRelease || !self.am_buyer();
        let contractual_txids_needed = self.state == TradeState::Init;

        Ok(ExchangedSigs {
            peers_warning_tx_buyer_input_partial_signature:
            peer_txs.warning.buyer_input_sig_ctx.my_partial_sig()?,
            peers_warning_tx_seller_input_partial_signature:
            peer_txs.warning.seller_input_sig_ctx.my_partial_sig()?,
            peers_redirect_tx_input_partial_signature:
            peer_txs.redirect.input_sig_ctx.my_partial_sig()?,
            peers_claim_tx_input_partial_signature:
            peer_txs.claim.input_sig_ctx.my_partial_sig()?,
            swap_tx_input_partial_signature:
            self.swap_tx.input_sig_ctx.my_partial_sig().ok().filter(|_| ready_to_release),
            swap_tx_input_sighash:
            self.swap_tx.input_sighash.as_ref(),
            contractual_txids:
            self.contractual_txids().ok().filter(|_| contractual_txids_needed),
        })
    }

    pub fn set_peer_partial_signatures_on_my_txs(&mut self, sigs: &ExchangedSigs<ByVal>) {
        let my_txs = if self.am_buyer() { &mut self.buyer_txs } else { &mut self.seller_txs };
        my_txs.warning.buyer_input_sig_ctx
            .set_peers_partial_sig(sigs.peers_warning_tx_buyer_input_partial_signature);
        my_txs.warning.seller_input_sig_ctx
            .set_peers_partial_sig(sigs.peers_warning_tx_seller_input_partial_signature);
        my_txs.redirect.input_sig_ctx
            .set_peers_partial_sig(sigs.peers_redirect_tx_input_partial_signature);
        my_txs.claim.input_sig_ctx
            .set_peers_partial_sig(sigs.peers_claim_tx_input_partial_signature);

        // NOTE: This passed field would normally be 'None' for the seller, as the buyer should redact the field
        // at the trade start and reveal it later, after payment is started, to prevent premature trade closure:
        sigs.swap_tx_input_partial_signature.map(|s| self.swap_tx.input_sig_ctx.set_peers_partial_sig(s));
    }

    pub fn aggregate_partial_signatures(&mut self) -> Result<()> {
        let my_txs = if self.am_buyer() { &mut self.buyer_txs } else { &mut self.seller_txs };
        my_txs.warning.buyer_input_sig_ctx.aggregate_partial_signatures()?;
        my_txs.warning.seller_input_sig_ctx.aggregate_partial_signatures()?;
        my_txs.redirect.input_sig_ctx.aggregate_partial_signatures()?;
        my_txs.claim.input_sig_ctx.aggregate_partial_signatures()?;
        if self.am_buyer() {
            // This forms a validated adaptor signature on the swap tx for the buyer, ensuring that the seller's
            // private key share is revealed if the swap tx is published. The seller doesn't get the full adaptor
            // signature (or the ordinary signature) until later on in the trade, when the buyer confirms payment:
            self.swap_tx.input_sig_ctx.aggregate_partial_signatures()?;
        }
        Ok(())
    }

    pub fn compute_my_signed_prepared_txs(&mut self) -> Result<()> {
        use MaybeScalar::Zero;
        let my_txs = if self.am_buyer() { &mut self.buyer_txs } else { &mut self.seller_txs };
        my_txs.warning.builder
            .set_buyer_input_signature(my_txs.warning.buyer_input_sig_ctx.compute_taproot_signature(Zero)?)
            .set_seller_input_signature(my_txs.warning.seller_input_sig_ctx.compute_taproot_signature(Zero)?)
            .compute_signed_tx()?;
        my_txs.redirect.builder
            .set_input_signature(my_txs.redirect.input_sig_ctx.compute_taproot_signature(Zero)?)
            .compute_signed_tx()?;
        my_txs.claim.builder
            .set_input_signature(my_txs.claim.input_sig_ctx.compute_taproot_signature(Zero)?)
            .compute_signed_tx()?;
        Ok(())
    }

    pub fn sign_deposit_psbt(&mut self) -> Result<bool> {
        // Check that we have all the prepared tx data we need:
        let my_txs = if self.am_buyer() { &self.buyer_txs } else { &self.seller_txs };
        my_txs.warning.builder.signed_tx()?;
        my_txs.redirect.builder.signed_tx()?;
        my_txs.claim.builder.signed_tx()?;
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
            self.deposit_tx.builder.sign_buyer_inputs(&mut *self.trade_wallet()?)?;
        } else {
            self.deposit_tx.builder.sign_seller_inputs(&mut *self.trade_wallet()?)?;
        }
        self.update_state(TradeState::Deposit)
    }

    pub fn deposit_psbt(&self) -> Result<&Psbt> {
        access!(self, _ if self.state >= Deposit)?;
        Ok(self.deposit_tx.builder.psbt()?)
    }

    pub fn combine_deposit_psbts(&mut self, other: Psbt) -> Result<()> {
        self.deposit_tx.builder.combine_psbts(other)?;
        Ok(())
    }

    pub fn signed_deposit_tx(&self) -> Result<Transaction> {
        access!(self, _ if self.state >= Deposit)?;
        Ok(self.deposit_tx.builder.signed_tx()?)
    }

    pub fn set_swap_tx_input_peers_partial_signature(&mut self, sig: PartialSignature) {
        self.swap_tx.input_sig_ctx.set_peers_partial_sig(sig);
    }

    pub fn aggregate_swap_tx_partial_signatures(&mut self) -> Result<()> {
        self.swap_tx.input_sig_ctx.aggregate_partial_signatures()?;
        Ok(())
    }

    pub fn my_private_key_share_for_peer_output(&self) -> Result<&Scalar> {
        access!(self, SellerReadyToRelease | TradeClosed(Cooperative))?;
        Ok(self.keys.peers_payout_ctx().my_key_share()?.prv_key()?)
    }

    pub fn set_peer_private_key_share_for_my_output(&mut self, prv_key_share: Scalar) -> Result<()> {
        self.keys.my_payout_ctx_mut().set_peers_prv_key(prv_key_share)?;
        Ok(())
    }

    pub fn aggregate_private_keys_for_my_output(&mut self) -> Result<&Scalar> {
        Ok(self.keys.my_payout_ctx_mut().aggregate_prv_key_shares()?)
    }

    pub fn compute_signed_swap_tx(&mut self) -> Result<()> {
        let adaptor_secret = (*self.keys.adaptor_key_share()?.prv_key()?).into();
        self.swap_tx.builder
            .set_input_signature(self.swap_tx.input_sig_ctx.compute_taproot_signature(adaptor_secret)?)
            .compute_signed_tx()?;
        Ok(())
    }

    pub fn signed_swap_tx(&self) -> Result<&Transaction> {
        access!(self, TradeClosed(Forced))?;
        Ok(self.swap_tx.builder.signed_tx()?)
    }

    pub fn recover_seller_private_key_share_for_buyer_output(&mut self, swap_tx: &Transaction) -> Result<()> {
        if self.am_buyer() {
            let swap_tx_input = self.deposit_tx.builder.seller_payout()?;
            let input_signature = swap_tx.find_key_spend_signature(swap_tx_input)?;
            let adaptor_secret = self.swap_tx.input_sig_ctx.reveal_adaptor_secret(input_signature)?;
            self.keys.buyer_payout_ctx.set_peers_prv_key(adaptor_secret)?;
        }
        Ok(())
    }

    pub fn compute_signed_penalty_tx(&mut self) -> Result<()> {
        self.penalty_tx.input_sig_ctx.sign_solo(self.penalty_tx.builder.input_sighash()?,
            *self.keys.my_payout_ctx_mut().peers_key_share()?.prv_key()?)?;
        self.penalty_tx.builder
            .set_input_signature(self.penalty_tx.input_sig_ctx.compute_taproot_signature(MaybeScalar::Zero)?)
            .compute_signed_tx()?;
        Ok(())
    }

    pub fn signed_penalty_tx(&self) -> Result<&Transaction> {
        Ok(self.penalty_tx.builder.signed_tx()?)
    }

    pub fn set_buyer_ready_to_release(&mut self) -> Result<bool> {
        // Regardless of role, make sure we have the Swap Tx adaptor signature before proceeding:
        self.swap_tx.input_sig_ctx.aggregated_sig()?;
        self.update_state(TradeState::BuyerReadyToRelease)
    }

    pub fn set_seller_ready_to_release(&mut self) -> Result<bool> {
        // NOTE: It is somewhat unsafe for the seller to start closing the trade near to (or after)
        //  the timelock expiry of the buyer's Warning Tx, since in case of an unresponsive buyer
        //  and subsequent force-closure, the Swap Tx could then be replaced by it, without the
        //  seller realising, but the seller won't have a signed Penalty Tx in that case.
        // TODO: Consider adding a protection against such late closures.

        // Regardless of role, make sure we have the Swap Tx final signature before proceeding:
        self.swap_tx.builder.signed_tx()?;
        self.update_state(TradeState::SellerReadyToRelease)
    }

    pub fn close_trade(&mut self, closure_type: ClosureType) -> Result<bool> {
        match closure_type {
            ClosureType::Forced if !self.am_buyer() => {
                self.swap_tx.builder.signed_tx()?;
                // TODO: Make sure we are committed to broadcasting the Swap Tx.
            }
            ClosureType::Cooperative | ClosureType::Forced => {
                // TODO: Should export the private key to the wallet, not just check we've got it:
                self.keys.my_payout_ctx_mut().aggregated_key()?.prv_key()?;
                self.penalty_tx.builder.signed_tx()?;
                // TODO: If forced closure, make sure we've seen the Swap Tx on the network.
            }
            ClosureType::Custom => {
                self.custom_payout_tx.builder.signed_tx()?;
                // TODO: Make sure we are committed to broadcasting the Custom Payout Tx, unless we
                //  have already seen it on the network.
            }
            // No call starts a warning, redirect, claim or penalty transaction yet, so these
            // closure types cannot be reached.
            ClosureType::BuyersRedirect
            | ClosureType::SellersRedirect
            | ClosureType::BuyersClaim
            | ClosureType::SellersClaim
            | ClosureType::BuyersPenaltyTx
            | ClosureType::SellersPenaltyTx => {
                return Err(ProtocolErrorKind::UnsupportedClosureType(closure_type));
            }
        }
        self.update_state(TradeState::TradeClosed(closure_type))
    }

    pub fn set_custom_payout_tx_fee_rate(&mut self, fee_rate: FeeRate) {
        self.custom_payout_tx.builder.set_fee_rate(fee_rate);
    }

    pub fn set_sellers_custom_payout_amount_excluding_fee(&mut self, amount: Amount) {
        self.custom_payout_tx.builder.set_seller_payout_amount_excluding_fee(amount);
    }

    pub fn compute_custom_payout_tx(&mut self) -> Result<()> {
        self.custom_payout_tx.builder
            .set_buyer_input(self.deposit_tx.builder.buyer_payout()?.clone())
            .set_seller_input(self.deposit_tx.builder.seller_payout()?.clone())
            .set_buyer_payout_address(self.buyer_txs.claim.builder.payout_address()?.clone())
            .set_seller_payout_address(self.seller_txs.claim.builder.payout_address()?.clone())
            .compute_unsigned_tx()?;
        Ok(())
    }

    pub fn sign_custom_payout_psbt(&mut self) -> Result<bool> {
        self.custom_payout_tx.builder.sign_partial(&mut *self.trade_wallet()?)?;
        self.update_state(TradeState::CustomPayoutSigned)
    }

    pub fn custom_payout_psbt(&self) -> Result<&Psbt> {
        access!(self, CustomPayoutSigned | TradeClosed(Custom))?;
        Ok(self.custom_payout_tx.builder.psbt()?)
    }

    pub fn combine_custom_payout_psbts(&mut self, other: Psbt) -> Result<()> {
        self.custom_payout_tx.builder.combine_psbts(other)?;
        Ok(())
    }

    pub fn signed_custom_payout_tx(&self) -> Result<Transaction> {
        access!(self, CustomPayoutSigned | TradeClosed(Custom))?;
        Ok(self.custom_payout_tx.builder.signed_tx()?)
    }
}

impl ArbitrationTxs {
    fn compute_unsigned_warning_tx(&mut self, deposit_tx: &DepositTx) -> Result<()> {
        self.warning.builder
            .set_buyer_input(deposit_tx.builder.buyer_payout()?.clone())
            .set_seller_input(deposit_tx.builder.seller_payout()?.clone())
            .compute_unsigned_tx()?;
        Ok(())
    }

    fn compute_unsigned_redirect_and_claim_txs(&mut self, peer: &Self) -> Result<()> {
        self.redirect.builder
            .set_input(peer.warning.builder.escrow()?.clone())
            .compute_unsigned_tx()?;
        self.claim.builder
            .set_input(self.warning.builder.escrow()?.clone())
            .compute_unsigned_tx()?;
        Ok(())
    }
}

impl Keys {
    const fn my_payout_ctx_mut(&mut self) -> &mut KeyCtx {
        if self.am_buyer { &mut self.buyer_payout_ctx } else { &mut self.seller_payout_ctx }
    }

    const fn peers_payout_ctx(&self) -> &KeyCtx {
        if self.am_buyer { &self.seller_payout_ctx } else { &self.buyer_payout_ctx }
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
            [self.buyer_payout_ctx.my_key_share()?, self.seller_payout_ctx.peers_key_share()?]
        } else {
            [self.buyer_payout_ctx.peers_key_share()?, self.seller_payout_ctx.my_key_share()?]
        })
    }

    fn multisig_script_keys(&self) -> Result<[&XOnlyPublicKey; 2]> {
        (|| Some(if self.am_buyer {
            [self.my_multisig_script_key.as_ref()?, self.peers_multisig_script_key.as_ref()?]
        } else {
            [self.peers_multisig_script_key.as_ref()?, self.my_multisig_script_key.as_ref()?]
        }))().ok_or(ProtocolErrorKind::MissingScriptKey)
    }

    fn internal_keys(&self) -> Result<[XOnlyPublicKey; 2]> {
        Ok([self.buyer_payout_ctx.aggregated_key()?, self.seller_payout_ctx.aggregated_key()?]
            .map(|p| p.pub_key().to_public_key().into()))
    }
}

type Result<T, E = ProtocolErrorKind> = std::result::Result<T, E>;

#[derive(Error, Debug)]
#[error(transparent)]
#[non_exhaustive]
pub enum ProtocolErrorKind {
    #[error("missing trade wallet")]
    MissingTradeWallet,
    #[error("missing script key")]
    MissingScriptKey,
    #[error("access denied")]
    AccessDenied,
    #[error("trade with id {0} already exists")]
    DuplicateTradeId(String),
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
    AddressParse(#[from] bdk_wallet::bitcoin::address::ParseError),
    Transaction(#[from] protocol::transaction::TransactionErrorKind),
    Multisig(#[from] protocol::multisig::MultisigErrorKind),
    Wallet(#[from] wallet::error::WalletErrorKind),
}

/// Unit tests of the trade engine without the gRPC service. Two trade models, one per trader,
/// exchange their data directly. The test IDs (G1, G4, ...) refer to section 9.4 of
/// `concept/CompletionPlan.md`, the F numbers to its findings.
///
/// The claim nonces and the claim signatures are passed on like all other fields, but no test
/// changes or checks them, because decision D3 of the plan removes them.
#[cfg(test)]
mod tests {
    use bdk_wallet::bitcoin::relative;
    use protocol::multisig::MultisigErrorKind;

    use super::*;

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

    const ROLE_PAIRS: [(Role, Role); 4] = [
        (Role::BuyerAsTaker, Role::SellerAsMaker),
        (Role::BuyerAsMaker, Role::SellerAsTaker),
        (Role::SellerAsTaker, Role::BuyerAsMaker),
        (Role::SellerAsMaker, Role::BuyerAsTaker),
    ];

    fn receivers(receivers: [(&str, u64); 3]) -> Vec<Receiver<NetworkUnchecked>> {
        receivers
            .into_iter()
            .map(|(address, amount)| Receiver {
                address: address.parse().expect("valid address"),
                amount: Amount::from_sat(amount),
            })
            .collect()
    }

    /// The engine calls of the `InitTrade` handler.
    fn new_trade_model(role: Role) -> TradeModel {
        let mut trade_model = TradeModel::new("trade".to_owned(), role);
        trade_model
            .init_my_key_shares()
            .expect("key shares can be created");
        trade_model
    }

    fn sent_key_shares(trade_model: &TradeModel) -> ExchangedKeys<'static, ByVal> {
        let key_shares = trade_model.my_key_shares().expect("key shares exist");
        ExchangedKeys {
            buyer_payout: *key_shares.buyer_payout,
            seller_payout: *key_shares.seller_payout,
            multisig_script: *key_shares.multisig_script,
        }
    }

    /// The engine calls of the `GetNonceShares` handler.
    fn receive_key_shares(
        trade_model: &mut TradeModel,
        peers_key_shares: &ExchangedKeys<ByVal>,
    ) -> Result<()> {
        trade_model.set_peer_key_shares(peers_key_shares);
        trade_model.aggregate_key_shares()?;
        trade_model.set_trade_amount(Amount::from_sat(200_000));
        trade_model.set_buyers_security_deposit(Amount::from_sat(30_000));
        trade_model.set_sellers_security_deposit(Amount::from_sat(30_000));
        trade_model.set_deposit_tx_fee_rate(FeeRate::from_sat_per_kwu(3_125));
        trade_model.set_prepared_tx_fee_rate(FeeRate::from_sat_per_kwu(2_500));
        trade_model.set_trade_fee_receiver(None)?;
        trade_model.init_my_addresses()?;
        trade_model.init_my_half_deposit_psbt()?;
        trade_model.init_my_nonce_shares()
    }

    type SentNonceShares = (
        Psbt,
        ExchangedAddresses<'static, ByVal, NetworkUnchecked>,
        ExchangedNonces<'static, ByVal>,
    );

    fn sent_nonce_shares(trade_model: &TradeModel) -> SentNonceShares {
        let half_deposit_psbt = trade_model
            .my_half_deposit_psbt()
            .expect("PSBT exists")
            .clone();
        let addresses = trade_model.my_addresses().expect("addresses exist");
        let addresses = ExchangedAddresses {
            warning_tx_fee_bump: addresses.warning_tx_fee_bump.as_unchecked().clone(),
            redirect_tx_fee_bump: addresses.redirect_tx_fee_bump.as_unchecked().clone(),
            claim_tx_payout: addresses.claim_tx_payout.as_unchecked().clone(),
        };
        let nonces = trade_model.my_nonce_shares().expect("nonce shares exist");
        let nonces = ExchangedNonces {
            swap_tx_input: nonces.swap_tx_input.clone(),
            buyers_warning_tx_buyer_input: nonces.buyers_warning_tx_buyer_input.clone(),
            buyers_warning_tx_seller_input: nonces.buyers_warning_tx_seller_input.clone(),
            sellers_warning_tx_buyer_input: nonces.sellers_warning_tx_buyer_input.clone(),
            sellers_warning_tx_seller_input: nonces.sellers_warning_tx_seller_input.clone(),
            buyers_redirect_tx_input: nonces.buyers_redirect_tx_input.clone(),
            sellers_redirect_tx_input: nonces.sellers_redirect_tx_input.clone(),
            buyers_claim_tx_input: nonces.buyers_claim_tx_input.clone(),
            sellers_claim_tx_input: nonces.sellers_claim_tx_input.clone(),
        };
        (half_deposit_psbt, addresses, nonces)
    }

    /// The engine calls of the `GetPartialSignatures` handler, without the ready-to-release flag.
    fn receive_nonce_shares(
        trade_model: &mut TradeModel,
        (peers_half_deposit_psbt, peers_addresses, peers_nonce_shares): SentNonceShares,
        redirection_receivers: [(&str, u64); 3],
    ) -> Result<()> {
        trade_model.set_peer_half_deposit_psbt(peers_half_deposit_psbt);
        trade_model.compute_unsigned_deposit_tx()?;
        trade_model.set_redirection_receivers(
            receivers(redirection_receivers)
                .into_iter()
                .map(Ok::<_, ProtocolErrorKind>),
        )?;
        trade_model.check_redirect_tx_params()?;
        trade_model.set_peer_addresses(peers_addresses)?;
        trade_model.compute_unsigned_prepared_txs()?;
        trade_model.set_peer_nonce_shares(peers_nonce_shares);
        trade_model.aggregate_nonce_shares()?;
        trade_model.sign_partial()
    }

    fn sent_partial_signatures(trade_model: &TradeModel) -> ExchangedSigs<'static, ByVal> {
        let sigs = trade_model
            .my_partial_signatures_on_peer_txs()
            .expect("signatures exist");
        ExchangedSigs {
            peers_warning_tx_buyer_input_partial_signature: *sigs
                .peers_warning_tx_buyer_input_partial_signature,
            peers_warning_tx_seller_input_partial_signature: *sigs
                .peers_warning_tx_seller_input_partial_signature,
            peers_redirect_tx_input_partial_signature: *sigs
                .peers_redirect_tx_input_partial_signature,
            peers_claim_tx_input_partial_signature: *sigs.peers_claim_tx_input_partial_signature,
            swap_tx_input_partial_signature: sigs.swap_tx_input_partial_signature.copied(),
            swap_tx_input_sighash: sigs.swap_tx_input_sighash.copied(),
            contractual_txids: None,
        }
    }

    /// The engine calls of the `SignDepositTx` handler.
    fn receive_partial_signatures(
        trade_model: &mut TradeModel,
        peers_partial_signatures: &ExchangedSigs<ByVal>,
    ) -> Result<()> {
        if trade_model.am_buyer() {
            let sighash = peers_partial_signatures
                .swap_tx_input_sighash
                .expect("seller sends the sighash of the swap transaction");
            trade_model.sign_swap_tx_input_partial(sighash)?;
        }
        trade_model.set_peer_partial_signatures_on_my_txs(peers_partial_signatures);
        trade_model.aggregate_partial_signatures()?;
        trade_model.compute_my_signed_prepared_txs()?;
        trade_model.sign_deposit_psbt()?;
        Ok(())
    }

    /// Two trade models that exchanged key shares, nonce shares and partial signatures, but did
    /// not yet process the partial signatures of the peer. Both are in state `Init`.
    fn trade_models_before_deposit_signature(
        (my_role, peers_role): (Role, Role),
        peers_redirection_receivers: [(&str, u64); 3],
        prepare_peer: impl FnOnce(&mut TradeModel),
    ) -> (TradeModel, TradeModel) {
        let mut me = new_trade_model(my_role);
        let mut peer = new_trade_model(peers_role);
        prepare_peer(&mut peer);
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

    fn random_partial_signature() -> PartialSignature {
        Scalar::random(&mut rand::rng()).into()
    }

    /// Checks that the trade model did not sign the deposit transaction.
    #[track_caller]
    fn assert_deposit_not_signed(trade_model: &TradeModel) {
        assert_eq!(trade_model.state(), TradeState::Init);
        assert!(matches!(
            trade_model.deposit_psbt(),
            Err(ProtocolErrorKind::AccessDenied)
        ));
        let psbt = trade_model
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

    #[track_caller]
    fn assert_invalid_signature(result: &Result<()>, context: &str) {
        assert!(
            matches!(
                result,
                Err(ProtocolErrorKind::Multisig(MultisigErrorKind::Verify(_)))
            ),
            "{context}: unexpected result {result:?}"
        );
    }

    /// The setup with valid data succeeds for every role. This checks the helpers of this module.
    #[test]
    fn valid_setup_signs_the_deposit() {
        for roles in ROLE_PAIRS {
            let (mut me, mut peer) =
                trade_models_before_deposit_signature(roles, REDIRECTION_RECEIVERS, |_| {});
            let peers_partial_signatures = sent_partial_signatures(&peer);
            receive_partial_signatures(&mut peer, &sent_partial_signatures(&me))
                .expect("valid signatures");
            receive_partial_signatures(&mut me, &peers_partial_signatures)
                .expect("valid signatures");
            assert_eq!(me.state(), TradeState::Deposit);
            assert_eq!(peer.state(), TradeState::Deposit);
            assert_eq!(
                me.deposit_psbt().expect("signed").unsigned_tx,
                peer.deposit_psbt().expect("signed").unsigned_tx
            );
        }
    }

    /// G1: the peer uses one key share for both payouts.
    #[test]
    #[ignore = "G1: equal key shares of the peer for both payouts are accepted"]
    fn g1_peers_key_shares_are_equal() {
        for (my_role, peers_role) in ROLE_PAIRS {
            let mut me = new_trade_model(my_role);
            let peers_key_shares = sent_key_shares(&new_trade_model(peers_role));
            let equal_key_shares = ExchangedKeys {
                seller_payout: peers_key_shares.buyer_payout,
                ..peers_key_shares
            };
            assert!(
                receive_key_shares(&mut me, &equal_key_shares).is_err(),
                "{my_role:?}"
            );
            assert_eq!(me.state(), TradeState::Init);
        }
    }

    /// G1: the peer sends our own key share back as its key share for the same payout.
    #[test]
    #[ignore = "G1: a key share of the peer that equals our own key share is accepted"]
    fn g1_peers_key_share_is_equal_to_own() {
        for (my_role, peers_role) in ROLE_PAIRS {
            for for_buyer_payout in [true, false] {
                let mut me = new_trade_model(my_role);
                let my_key_shares = sent_key_shares(&me);
                let peers_key_shares = sent_key_shares(&new_trade_model(peers_role));
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
                assert!(
                    receive_key_shares(&mut me, &key_shares_with_own).is_err(),
                    "{my_role:?}"
                );
                assert_eq!(me.state(), TradeState::Init);
            }
        }
    }

    /// G4: the peer uses one nonce for two signatures.
    #[test]
    #[ignore = "G4: a nonce of the peer that is used for two signatures is accepted"]
    fn g4_peers_nonce_is_repeated() {
        for (my_role, peers_role) in ROLE_PAIRS {
            let mut me = new_trade_model(my_role);
            let mut peer = new_trade_model(peers_role);
            receive_key_shares(&mut me, &sent_key_shares(&peer)).expect("valid key shares");
            receive_key_shares(&mut peer, &sent_key_shares(&me)).expect("valid key shares");

            let (half_deposit_psbt, addresses, nonces) = sent_nonce_shares(&peer);
            let repeated_nonces = ExchangedNonces {
                buyers_redirect_tx_input: nonces.sellers_redirect_tx_input.clone(),
                ..nonces
            };
            let result = receive_nonce_shares(
                &mut me,
                (half_deposit_psbt, addresses, repeated_nonces),
                REDIRECTION_RECEIVERS,
            );
            assert!(result.is_err(), "{my_role:?}");
            assert_eq!(me.state(), TradeState::Init);
        }
    }

    /// G5: an invalid partial signature of the peer for the warning transaction, the redirect
    /// transaction or the swap transaction makes the aggregation fail. The deposit transaction is
    /// not signed.
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
                    trade_models_before_deposit_signature(roles, REDIRECTION_RECEIVERS, |_| {});
                if name == "swap transaction" && !me.am_buyer() {
                    continue;
                }
                let mut peers_partial_signatures = sent_partial_signatures(&peer);
                change(&mut peers_partial_signatures);

                let result = receive_partial_signatures(&mut me, &peers_partial_signatures);
                assert_invalid_signature(&result, &format!("{:?}, {name}", roles.0));
                assert_deposit_not_signed(&me);
            }
        }
    }

    /// G6: the peer signed transactions with other outputs. The peer computed its redirect
    /// transactions with other receiver amounts, so its partial signature is for another message.
    #[test]
    fn g6_partial_signature_for_other_outputs() {
        for roles in ROLE_PAIRS {
            let (mut me, peer) =
                trade_models_before_deposit_signature(roles, OTHER_REDIRECTION_RECEIVERS, |_| {});

            let result = receive_partial_signatures(&mut me, &sent_partial_signatures(&peer));
            assert_invalid_signature(&result, &format!("{:?}", roles.0));
            assert_deposit_not_signed(&me);
        }
    }

    /// G6: the peer signed transactions with another lock time. The peer computed its warning
    /// transactions with another lock time, so its partial signatures are for other messages.
    #[test]
    fn g6_partial_signature_for_other_lock_time() {
        for roles in ROLE_PAIRS {
            let (mut me, peer) =
                trade_models_before_deposit_signature(roles, REDIRECTION_RECEIVERS, |peer| {
                    // The lock time of a builder cannot be changed, so replace the builders.
                    for txs in [&mut peer.buyer_txs, &mut peer.seller_txs] {
                        txs.warning.builder = WarningTxBuilder::default();
                        txs.warning
                            .builder
                            .set_lock_time(relative::LockTime::from_height(6));
                    }
                });

            let result = receive_partial_signatures(&mut me, &sent_partial_signatures(&peer));
            assert_invalid_signature(&result, &format!("{:?}", roles.0));
            assert_deposit_not_signed(&me);
        }
    }

    /// F4: `close_trade` returns an error for the closure types that are not implemented. It
    /// does not panic and does not change the state.
    #[test]
    fn f4_close_trade_with_dispute_closure_type() {
        for roles in ROLE_PAIRS {
            let (mut me, peer) =
                trade_models_before_deposit_signature(roles, REDIRECTION_RECEIVERS, |_| {});
            receive_partial_signatures(&mut me, &sent_partial_signatures(&peer))
                .expect("valid signatures");
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
                        me.close_trade(closure_type),
                        Err(ProtocolErrorKind::UnsupportedClosureType(c)) if c == closure_type
                    ),
                    "{closure_type:?}"
                );
                assert_eq!(me.state(), TradeState::Deposit);
            }
        }
    }
}
