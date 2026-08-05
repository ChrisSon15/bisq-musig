use std::str::FromStr as _;

use bdk_wallet::bitcoin::{
    Address, Amount, FeeRate, Network, OutPoint, Psbt, Script, ScriptBuf, TapNodeHash, Transaction,
    TxOut, Txid, XOnlyPublicKey, relative,
};
use bdk_wallet::miniscript::{DefiniteDescriptorKey, Descriptor};
use chain::ChainApi;
use musig2::secp::{MaybeScalar, Point};
use musig2::{PartialSignature, PubNonce};
use wallet::protocol_wallet_api::ProtocolWalletApi;

use crate::multisig::{KeyCtx, PointExt as _, SigCtx};
use crate::receiver::{Receiver, ReceiverList};
use crate::script_paths;
use crate::transaction::{
    ClaimTxBuilder, DepositTxBuilder, RedirectTxBuilder, SwapTxBuilder, TransactionExt as _,
    WarningTxBuilder,
};

/// Type-erased wallet handle used by the trade protocol. The concrete wallet may be a
/// `MemWallet`, a `BMPWallet<Connection>`, or any other type that implements [`ProtocolWalletApi`].
pub type BoxedTradeWallet = Box<dyn ProtocolWalletApi + Send>;

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
#[expect(clippy::exhaustive_enums)]
pub enum ProtocolRole {
    Seller,
    Buyer,
}

impl ProtocolRole {
    const fn other(self) -> Self {
        match self {
            Self::Seller => Self::Buyer,
            Self::Buyer => Self::Seller,
        }
    }
}

#[derive(Debug)]
pub struct Round1Parameter {
    // DepositTx --------
    pub p_a: Point,
    pub q_a: Point,
    pub script_key: XOnlyPublicKey,
    pub dep_part_psbt: Psbt,
    // Swap Tx -----
    // public nonce
    // Seller address where to send the swap amount to -- only set from Seller:
    pub swap_script: Option<ScriptBuf>,
    pub warn_anchor_spend: ScriptBuf,
    pub claim_spend: ScriptBuf,
    pub redirect_anchor_spend: ScriptBuf,
}

#[derive(Debug)]
pub struct Round2Parameter {
    // DepositTx --------
    pub p_agg: Point,
    pub q_agg: Point,
    // SwapTx --------------
    // partial adaptive  signature for SwapTx
    pub swap_pub_nonce: PubNonce,
    pub warn_alice_p_nonce: PubNonce,
    pub warn_alice_q_nonce: PubNonce,
    pub warn_bob_p_nonce: PubNonce,
    pub warn_bob_q_nonce: PubNonce,
    pub claim_alice_nonce: PubNonce,
    pub claim_bob_nonce: PubNonce,
    pub redirect_alice_nonce: PubNonce,
    pub redirect_bob_nonce: PubNonce,
}

#[derive(Debug)]
pub struct Round3Parameter {
    // DepositTx --------
    // only for verification / fast fail
    pub deposit_txid: Txid,
    // SwapTx --------------
    // aggregated adaptive signature for SwapTx,
    pub swap_part_sig: PartialSignature,
    pub p_part_peer: PartialSignature,
    pub q_part_peer: PartialSignature,
    pub claim_part_sig: PartialSignature,
    pub redirect_part_sig: PartialSignature,
}

#[derive(Debug)]
pub struct Round4Parameter {
    pub deposit_tx_signed: Psbt,
}

/// this context is for the whole process and need to be persisted by the caller
pub struct BMPContext {
    // first of all, everything which is general to the protocol itself.
    // `funds` is type-erased so the protocol works against any [`TradeWallet`]
    // implementation (e.g. `MemWallet`, `BMPWallet`, mocks).
    pub funds: BoxedTradeWallet,
    pub chain: Box<dyn ChainApi>,
    pub role: ProtocolRole,
    pub seller_amount: Amount,
    pub buyer_amount: Amount,
}

pub struct BMPProtocol {
    pub ctx: BMPContext,
    // Point securing Seller deposit and trade amount:
    pub p_tik: KeyCtx,
    // Point securing Buyer deposit:
    pub q_tik: KeyCtx,
    script_key_me: Option<XOnlyPublicKey>,
    script_key_peer: Option<XOnlyPublicKey>,
    pub deposit_tx: DepositTx,
    // which round are we in:
    round: u8,
    pub swap_tx: SwapTx,
    pub warning_tx_me: WarningTx,
    warning_tx_peer: WarningTx,
    pub claim_tx_me: ClaimTx,
    claim_tx_peer: ClaimTx,
    pub redirect_tx_me: RedirectTx,
    redirect_tx_peer: RedirectTx,
}

impl BMPContext {
    pub fn new(
        chain: Box<dyn ChainApi>,
        funds: BoxedTradeWallet,
        role: ProtocolRole,
        seller_amount: Amount,
        buyer_amount: Amount,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            funds,
            chain,
            role,
            seller_amount,
            buyer_amount,
        })
    }

    const fn am_buyer(&self) -> bool { matches!(self.role, ProtocolRole::Buyer) }
}

impl BMPProtocol {
    pub fn new(ctx: BMPContext) -> anyhow::Result<Self> {
        let role = ctx.role;
        let [mut p_tik, mut q_tik] = [KeyCtx::default(), KeyCtx::default()];
        p_tik.init_my_key_share();
        q_tik.init_my_key_share();
        Ok(Self {
            ctx,
            p_tik,
            q_tik,
            script_key_me: None,
            script_key_peer: None,
            deposit_tx: DepositTx::new(),
            round: 0,
            swap_tx: SwapTx::new(role),
            warning_tx_me: WarningTx::new(role),
            warning_tx_peer: WarningTx::new(role.other()),
            claim_tx_me: ClaimTx::new(),
            claim_tx_peer: ClaimTx::new(),
            redirect_tx_me: RedirectTx::new(),
            redirect_tx_peer: RedirectTx::new(),
        })
    }

    pub fn round1(&mut self) -> anyhow::Result<Round1Parameter> {
        self.check_round(1);

        let dep_part_psbt = self.deposit_tx.generate_part_tx(&mut self.ctx)?;
        let swap_script = self.swap_tx.spend_condition(&mut self.ctx)?;
        let warn_anchor_spend = self.ctx.funds.new_address()?.script_pubkey();
        self.warning_tx_me.anchor_spend = Some(warn_anchor_spend.clone());

        // ClaimTx
        let claim_spend = self.ctx.funds.new_address()?.script_pubkey();
        self.claim_tx_me.claim_spend = Some(claim_spend.clone());

        // RedirectTx
        let redirect_anchor_spend = self.ctx.funds.new_address()?.script_pubkey();
        self.redirect_tx_me.anchor_spend = Some(redirect_anchor_spend.clone());

        let script_key = self.ctx.funds.new_internal_key()?;
        self.script_key_me = Some(script_key);

        Ok(Round1Parameter {
            p_a: *self.p_tik.my_key_share()?.pub_key(),
            q_a: *self.q_tik.my_key_share()?.pub_key(),
            script_key,
            dep_part_psbt,
            swap_script,
            warn_anchor_spend,
            claim_spend,
            redirect_anchor_spend,
        })
    }

    #[expect(clippy::similar_names, reason = "easy to distinguish local variable names in this case")]
    pub fn round2(&mut self, bob: Round1Parameter) -> anyhow::Result<Round2Parameter> {
        self.check_round(2);
        assert_ne!(bob.p_a, bob.q_a, "Bob is sending the same point for P' and Q'.");
        tracing::debug!(
            "The {:?} sellers secret for P_Tik is {:?}.",
            self.ctx.role,
            self.p_tik.my_key_share()?.prv_key()?
        );

        // key Aggregation -----
        self.p_tik.set_peers_pub_key(bob.p_a);
        self.q_tik.set_peers_pub_key(bob.q_a);
        self.p_tik.aggregate_pub_key_shares()?;
        self.q_tik.aggregate_pub_key_shares()?;
        // now we have the aggregated key
        // so we can construct the Deposit Tx
        self.script_key_peer = Some(bob.script_key);
        let [buyer_pub_key, seller_pub_key] = self.script_keys()?;
        self.deposit_tx.build_and_merge_tx(&mut self.ctx, bob.dep_part_psbt, &self.p_tik, &self.q_tik,
            buyer_pub_key, seller_pub_key)?;
        self.warning_tx_me.build(&mut self.ctx, &self.p_tik, &self.q_tik, &self.deposit_tx)?;
        self.warning_tx_peer.anchor_spend = Some(bob.warn_anchor_spend);
        self.warning_tx_peer.build(&mut self.ctx, &self.p_tik, &self.q_tik, &self.deposit_tx)?;
        let warn_alice_p_nonce = self.warning_tx_me.sig_p.my_nonce_share()?.clone();
        let warn_alice_q_nonce = self.warning_tx_me.sig_q.my_nonce_share()?.clone();
        let warn_bob_p_nonce = self.warning_tx_peer.sig_p.my_nonce_share()?.clone();
        let warn_bob_q_nonce = self.warning_tx_peer.sig_q.my_nonce_share()?.clone();

        let adaptor_point = *match self.ctx.role { // the seller's key for payout of seller deposit and trade amount is in question
            ProtocolRole::Seller => self.p_tik.my_key_share(),
            ProtocolRole::Buyer => self.p_tik.peers_key_share(),
        }?.pub_key();
        // given the DepositTx, we can create SwapTx for Alice.
        self.swap_tx.build(&self.q_tik, adaptor_point, &self.deposit_tx, bob.swap_script.as_deref())?;
        // let start the signing process for SwapTx already.
        let swap_pub_nonce = self.swap_tx.get_pub_nonce(); // could be one round earlier, if we solve secure nonce generation

        // ClaimTx
        let (tik, other_tik) = match self.ctx.role {
            ProtocolRole::Seller => (&self.q_tik, &self.p_tik),
            ProtocolRole::Buyer => (&self.p_tik, &self.q_tik)
        };
        self.claim_tx_me.build(tik, &self.warning_tx_me)?;
        let claim_alice_nonce = self.claim_tx_me.sig.my_nonce_share()?.clone();
        self.claim_tx_peer.claim_spend = Some(bob.claim_spend);
        self.claim_tx_peer.build(other_tik, &self.warning_tx_peer)?;
        let claim_bob_nonce = self.claim_tx_peer.sig.my_nonce_share()?.clone();

        // RedirectTx
        self.redirect_tx_me.build(other_tik, &self.warning_tx_peer)?; // RedirectTx overcrosses; Alice references Bob's WarningTx
        let redirect_alice_nonce = self.redirect_tx_me.sig.my_nonce_share()?.clone();
        self.redirect_tx_peer.anchor_spend = Some(bob.redirect_anchor_spend);
        self.redirect_tx_peer.build(tik, &self.warning_tx_me)?;
        let redirect_bob_nonce = self.redirect_tx_peer.sig.my_nonce_share()?.clone();

        Ok(Round2Parameter {
            p_agg: *self.p_tik.aggregated_key()?.pub_key(),
            q_agg: *self.q_tik.aggregated_key()?.pub_key(),
            swap_pub_nonce,
            warn_alice_p_nonce,
            warn_alice_q_nonce,
            warn_bob_p_nonce,
            warn_bob_q_nonce,
            claim_alice_nonce,
            claim_bob_nonce,
            redirect_alice_nonce,
            redirect_bob_nonce,
        })
    }

    pub fn round3(&mut self, bob: Round2Parameter) -> anyhow::Result<Round3Parameter> {
        self.check_round(3);
        // actually this next test is not necessary, but double-checking and fast fail is always good
        // TODO since we are sending this only to validate, we could use a hash of it as well, optimization
        assert_eq!(bob.p_agg, *self.p_tik.aggregated_key()?.pub_key(), "Bob is sending the wrong P' for his aggregated key.");
        assert_eq!(bob.q_agg, *self.q_tik.aggregated_key()?.pub_key(), "Bob is sending the wrong Q' for his aggregated key.");

        let txid = self.deposit_tx.txid()?;
        // here we are building the partial signature of the SwapTx, note that there is only one SwapTx (for Alice)
        let swap_part_sig = self.swap_tx.build_partial_sig(bob.swap_pub_nonce)?;

        let [_p_part_me, _q_part_me] = self.warning_tx_me.build_partial_sig(bob.warn_bob_p_nonce, bob.warn_bob_q_nonce)?;

        let [p_part_peer, q_part_peer] = self.warning_tx_peer.build_partial_sig(bob.warn_alice_p_nonce, bob.warn_alice_q_nonce)?;
        // ClaimTx
        self.claim_tx_me.build_partial_sig(bob.claim_bob_nonce)?; // no need to send my partial sig to peer
        let claim_part_sig = self.claim_tx_peer.build_partial_sig(bob.claim_alice_nonce)?; // sign bobs transaction that I constructed

        // RedirectTx
        self.redirect_tx_me.build_partial_sig(bob.redirect_bob_nonce)?;
        let redirect_part_sig = self.redirect_tx_peer.build_partial_sig(bob.redirect_alice_nonce)?; // sign bobs transaction that I constructed

        Ok(Round3Parameter {
            deposit_txid: txid, // only for verification that we actually are on the same page
            swap_part_sig, // partial signature for SwapTx Alice (or None if we are Alice)
            p_part_peer, // partial signature for WarningTx Bob, input of p_tik
            q_part_peer,
            claim_part_sig,
            redirect_part_sig,
        })
    }

    #[expect(clippy::needless_pass_by_value, reason = "gives a safer & more consistent API")]
    pub fn round4(&mut self, bob: Round3Parameter) -> anyhow::Result<Round4Parameter> {
        self.check_round(4);
        dbg!(&bob);
        self.swap_tx.aggregate_sigs(bob.swap_part_sig)?;
        self.warning_tx_me.aggregate_sigs(bob.p_part_peer, bob.q_part_peer)?;
        self.claim_tx_me.aggregate_sigs(bob.claim_part_sig)?;
        self.redirect_tx_me.aggregate_sigs(bob.redirect_part_sig)?;
        // TODO final check
        self.deposit_tx.sign(&mut self.ctx)?;
        Ok(Round4Parameter {
            deposit_tx_signed: self.deposit_tx.merged_psbt()?.clone()
        })
    }

    pub fn round5(&mut self, bob: Round4Parameter) -> anyhow::Result<()> {
        self.check_round(5);
        self.deposit_tx.transfer_sig_and_broadcast(&mut self.ctx, bob.deposit_tx_signed)?;
        Ok(())
    }

    fn script_keys(&self) -> anyhow::Result<[XOnlyPublicKey; 2]> {
        (|| Some(if self.ctx.am_buyer() {
            [self.script_key_me?, self.script_key_peer?]
        } else {
            [self.script_key_peer?, self.script_key_me?]
        }))().ok_or_else(|| anyhow::anyhow!("Missing script key"))
    }

    fn check_round(&mut self, round: u8) {
        assert_eq!(self.round, round - 1, "round already done");
        self.round = round;
    }

    // ------- Debug --------
    pub(crate) fn _get_p_tik_agg(&self) -> Address {
        let r = self.p_tik.with_taproot_tweak(None).unwrap();
        r.p2tr_address(Network::Regtest)
    }
}

/// `RedirectTx` -- this redirects the funds from the `WarningTx` to the DAO.
/// This is expected if the traders have some sor of conflict, which they cannot resolve themselves.
/// One trader sends the `WarningTx`, the other trader answers by sending the `RedirectTx`.
/// If a redirectTx is not send within `t_2`, then the trader which sent the `WarningTx` can
/// send the `ClaimTx` and gets the hole funds for himself.
/// Since `RedirectTx` sends the funds to the DAO, it needs an anchors for the trader, so he
/// can raise the fees with CPFP to get it mined before `ClaimTx` can be broadcast.
///
/// `RedirectTx` Bob spends from `WarningTx` Alice, that's important.
/// Sending funds to the DAO is done by having a list of addresses (from contributors) and
/// percentages. (must add up to 100%)
#[derive(Default)]
pub struct RedirectTx {
    pub sig: SigCtx,
    pub builder: RedirectTxBuilder,
    pub anchor_spend: Option<ScriptBuf>,
}

impl RedirectTx {
    pub fn new() -> Self { Self::default() }

    fn build(&mut self, tik: &KeyCtx, warn_tx: &WarningTx) -> anyhow::Result<()> {
        self.sig.set_tweaked_key_ctx(tik.with_taproot_tweak(None)?);
        self.sig.init_my_nonce_share()?;

        let receiver_shares = Self::get_dao_bm();
        let fee_rate = FeeRate::from_sat_per_vb_u32(10); // TODO: feerates shall come from pricenodes

        let escrow_amount = warn_tx.builder.escrow()?.prevout.value;
        let available_amount_msat = RedirectTxBuilder::available_amount_msat(escrow_amount, fee_rate)?;

        let receivers = Receiver::compute_receivers_from_shares(receiver_shares, available_amount_msat, fee_rate)
            .ok_or(anyhow::anyhow!("Could not compute receiver list"))?;

        let t1 = relative::LockTime::from_height(1); // TODO: define as const and find a good value
        self.builder
            .set_input(warn_tx.builder.escrow()?)
            .set_receivers(receivers)
            .set_anchor_address(Address::from_script(self.anchor_spend.as_ref().unwrap(), Network::Regtest)?) // TODO: Improve.
            .set_lock_time(t1)
            .compute_unsigned_tx()?;
        Ok(())
    }

    fn build_partial_sig(&mut self, peer_nonce: PubNonce) -> anyhow::Result<PartialSignature> {
        let msg = self.builder.input_sighash()?;
        self.sig.set_peers_nonce_share(peer_nonce);
        self.sig.aggregate_nonce_shares()?;
        Ok(*self.sig.sign_partial(msg)?)
    }

    pub fn aggregate_sigs(&mut self, part_sig: PartialSignature) -> anyhow::Result<()> {
        self.sig.set_peers_partial_sig(part_sig);
        self.sig.aggregate_partial_signatures()?;

        // now stuff those signatures into the transaction
        self.builder
            .set_input_signature(self.sig.compute_taproot_signature(MaybeScalar::Zero)?)
            .compute_signed_tx()?;
        Ok(())
    }

    pub fn broadcast(&self, me: &BMPContext) -> anyhow::Result<Txid> {
        me.chain.transaction_broadcast(self.builder.signed_tx()?)
    }

    /// sum of all f64 must be 1
    //noinspection SpellCheckingInspection
    fn get_dao_bm() -> Vec<(Address, f64)> {
        // TODO this needs a real implementation, and check that sum of ratios is 1
        vec![
            (Address::from_str("bcrt1p88h9s6lq8jw3ehdlljp7sa85kwpp9lvyrl077twvjnackk4lxt0sffnlrk").unwrap().assume_checked(), 0.6),
            (Address::from_str("bcrt1phhl8d90r9haqwtvw2cv4ryjl8tlnqrv48nhpy7yyks5du6mr66xq5nlwhz").unwrap().assume_checked(), 0.4),
        ]
    }
}

/// `ClaimTx` -- One version for Alice and one for Bob.
/// If the other side will not react on the `WarningTx` (by sending the `RedirectTx`)
/// then Alice can claim the total amounts for herself.
#[derive(Default)]
pub struct ClaimTx {
    pub sig: SigCtx,
    pub builder: ClaimTxBuilder,
    pub claim_spend: Option<ScriptBuf>,
}

impl ClaimTx {
    pub fn new() -> Self { Self::default() }

    pub fn signed_tx(&self) -> anyhow::Result<&Transaction> { Ok(self.builder.signed_tx()?) }

    fn build(&mut self, tik: &KeyCtx, warn_tx: &WarningTx) -> anyhow::Result<()> {
        self.sig.set_tweaked_key_ctx(tik.with_taproot_tweak(None)?);
        self.sig.init_my_nonce_share()?;

        let t2 = relative::LockTime::from_height(2); // TODO: define as const and find a good value
        self.builder
            .set_input(warn_tx.builder.escrow()?)
            .set_payout_address(Address::from_script(self.claim_spend.as_ref().unwrap(), Network::Regtest)?) // TODO: Improve.
            .set_lock_time(t2)
            .set_fee_rate(FeeRate::from_sat_per_vb_u32(10)) // TODO: feerates shall come from pricenodes
            .compute_unsigned_tx()?;
        Ok(())
    }

    fn build_partial_sig(&mut self, peer_nonce: PubNonce) -> anyhow::Result<PartialSignature> {
        let msg = self.builder.input_sighash()?;
        self.sig.set_peers_nonce_share(peer_nonce);
        self.sig.aggregate_nonce_shares()?;
        Ok(*self.sig.sign_partial(msg)?)
    }

    pub fn aggregate_sigs(&mut self, part_sig: PartialSignature) -> anyhow::Result<()> {
        self.sig.set_peers_partial_sig(part_sig);
        self.sig.aggregate_partial_signatures()?;

        // now stuff those signatures into the transaction
        self.builder
            .set_input_signature(self.sig.compute_taproot_signature(MaybeScalar::Zero)?)
            .compute_signed_tx()?;
        Ok(())
    }

    pub fn broadcast(&self, me: &BMPContext) -> anyhow::Result<Txid> {
        me.chain.transaction_broadcast(self.signed_tx()?)
    }
}

/// `WarningTx` -- there is one version for Alice and one for Bob.
/// That means each party generates both transaction and sign them.
pub struct WarningTx {
    // is that my WarningTx? (mainly for safety checking):
    role: ProtocolRole,
    pub builder: WarningTxBuilder,
    // where to send the anchor sats to:
    pub anchor_spend: Option<ScriptBuf>,
    pub sig_p: SigCtx,
    pub sig_q: SigCtx,
}

impl WarningTx {
    pub fn unsigned_tx(&self) -> anyhow::Result<&Transaction> { Ok(self.builder.unsigned_tx()?) }

    pub fn signed_tx(&self) -> anyhow::Result<&Transaction> { Ok(self.builder.signed_tx()?) }

    pub fn funds_as_outpoint(&self) -> OutPoint { self.builder.escrow().unwrap().outpoint }

    pub fn funds_as_output(&self) -> TxOut { self.builder.escrow().unwrap().prevout }

    pub fn new(role: ProtocolRole) -> Self {
        Self {
            role,
            builder: WarningTxBuilder::default(),
            anchor_spend: None, // ctx.funds.next_unused_address().script_pubkey(),
            sig_p: SigCtx::default(),
            sig_q: SigCtx::default(),
        }
    }

    fn build(&mut self, ctx: &mut BMPContext, p_tik: &KeyCtx, q_tik: &KeyCtx, deposit_tx: &DepositTx) -> anyhow::Result<()> {
        self.sig_p.set_tweaked_key_ctx(p_tik.with_taproot_tweak(deposit_tx.merkle_root.as_ref())?);
        self.sig_p.init_my_nonce_share()?;
        self.sig_q.set_tweaked_key_ctx(q_tik.with_taproot_tweak(deposit_tx.merkle_root.as_ref())?);
        self.sig_q.init_my_nonce_share()?;

        //--------------------
        let key_spend = match self.role {
            ProtocolRole::Seller => q_tik,
            ProtocolRole::Buyer => p_tik
        }.with_taproot_tweak(None)?;

        let txid = self.builder
            .set_buyer_input(deposit_tx.builder.buyer_payout()?.clone())
            .set_seller_input(deposit_tx.builder.seller_payout()?.clone())
            .set_escrow_address(key_spend.p2tr_address(Network::Regtest))
            .set_anchor_address(Address::from_script(self.anchor_spend.as_ref().unwrap(), Network::Regtest)?) // TODO: Improve.
            .set_lock_time(relative::LockTime::ZERO)
            .set_fee_rate(FeeRate::from_sat_per_vb_u32(10)) // TODO: feerates shall come from pricenodes
            .compute_unsigned_tx()?
            .txid()?;

        dbg!(ctx.role, self.role, txid);
        Ok(())
    }

    fn build_partial_sig(&mut self, peer_nonce_p: PubNonce, peer_nonce_q: PubNonce) -> anyhow::Result<[PartialSignature; 2]> {
        let p_msg = self.builder.buyer_input_sighash()?;
        self.sig_p.set_peers_nonce_share(peer_nonce_p);
        self.sig_p.aggregate_nonce_shares()?;
        let p_part = *self.sig_p.sign_partial(p_msg)?;

        let q_msg = self.builder.seller_input_sighash()?;
        self.sig_q.set_peers_nonce_share(peer_nonce_q);
        self.sig_q.aggregate_nonce_shares()?;
        let q_part = *self.sig_q.sign_partial(q_msg)?;

        Ok([p_part, q_part])
    }

    pub fn aggregate_sigs(&mut self, p_part_sig: PartialSignature, q_part_sig: PartialSignature) -> anyhow::Result<()> {
        dbg!("agg p");
        self.sig_p.set_peers_partial_sig(p_part_sig);
        self.sig_p.aggregate_partial_signatures()?;
        dbg!("agg q");
        self.sig_q.set_peers_partial_sig(q_part_sig);
        self.sig_q.aggregate_partial_signatures()?;

        // now stuff those signatures into the transaction
        self.builder
            .set_buyer_input_signature(self.sig_p.compute_taproot_signature(MaybeScalar::Zero)?)
            .set_seller_input_signature(self.sig_q.compute_taproot_signature(MaybeScalar::Zero)?)
            .compute_signed_tx()?;
        Ok(())
    }

    pub fn broadcast(&self, me: &BMPContext) -> anyhow::Result<Txid> {
        me.chain.transaction_broadcast(self.signed_tx()?)
    }
}

/// Only the seller gets a `SwapTx`, this is the only asymmetric part of the p3
pub struct SwapTx {
    // this transaction is only for Alice, however even Bob will construct it for signing:
    pub role: ProtocolRole,
    pub builder: SwapTxBuilder,
    pub swap_spend: Option<ScriptBuf>,
    // SwapTx get funded by a adaptor MuSig2 signature
    pub fund_sig: SigCtx,
}

impl SwapTx {
    pub(crate) fn spend_condition(
        &mut self,
        ctx: &mut BMPContext,
    ) -> anyhow::Result<Option<ScriptBuf>> {
        self.swap_spend = match self.role {
            ProtocolRole::Seller => Some(ctx.funds.new_address()?.script_pubkey()),
            ProtocolRole::Buyer => None,
        };
        Ok(self.swap_spend.clone())
    }

    /// even though only the seller gets a `SwapTx` transaction, both parties are constructing the
    /// transaction and only the buyer will send the seller the signature.
    fn new(role: ProtocolRole) -> Self {
        Self {
            role,
            builder: SwapTxBuilder::default(),
            swap_spend: None,
            fund_sig: SigCtx::default(),
        }
    }

    pub fn unsigned_tx(&self) -> anyhow::Result<&Transaction> { Ok(self.builder.unsigned_tx()?) }

    pub fn get_pub_nonce(&self) -> PubNonce {
        self.fund_sig.my_nonce_share().unwrap().clone()
    }

    // round 1
    pub fn build(&mut self, q_tik: &KeyCtx, p_a: Point, deposit_tx: &DepositTx, swap_spend_opt: Option<&Script>) -> anyhow::Result<()> {
        self.fund_sig.set_tweaked_key_ctx(q_tik.with_taproot_tweak(deposit_tx.merkle_root.as_ref())?);
        // SwapTx is asymmetric, both parties need to agree on P_a being the public adaptor
        // P_a is the Public key which Alice (the seller) contributes to 2of2 Multisig to lock the deposit and trade amount in the DepositTx
        // if secret key of P_a is revealed to Bob, then we has both partial keys to it and is able to spend it.
        self.fund_sig.set_adaptor_point(p_a)?;
        self.fund_sig.init_my_nonce_share()?;
        let Some(use_spend) = (match self.role {
            ProtocolRole::Seller => self.swap_spend.as_deref(),
            ProtocolRole::Buyer => swap_spend_opt,
        }) else { panic!("No spend-condition from role {:?}", self.role) };

        self.builder
            .set_input(deposit_tx.builder.seller_payout()?.clone())
            .set_payout_address(Address::from_script(use_spend, Network::Regtest)?) // TODO: Improve.
            .disable_lock_time()
            .set_fee_rate(FeeRate::from_sat_per_vb_u32(10)) // TODO: feerates shall come from pricenodes
            .compute_unsigned_tx()?;
        Ok(())
    }

    pub fn build_partial_sig(&mut self, other_nonce: PubNonce) -> anyhow::Result<PartialSignature> {
        let msg = self.builder.input_sighash()?;
        self.fund_sig.set_peers_nonce_share(other_nonce);
        self.fund_sig.aggregate_nonce_shares()?;
        Ok(*self.fund_sig.sign_partial(msg)?)
    }

    pub fn aggregate_sigs(&mut self, other_sig: PartialSignature) -> anyhow::Result<()> {
        self.fund_sig.set_peers_partial_sig(other_sig);
        self.fund_sig.aggregate_partial_signatures()?;
        Ok(())
    }

    pub fn sign(&mut self, p_tik: &KeyCtx) -> anyhow::Result<Transaction> {
        // only seller can do this
        if self.role == ProtocolRole::Seller {
            let adaptor_secret = (*p_tik.my_key_share()?.prv_key()?).into();
            let tx = self.builder
                .set_input_signature(self.fund_sig.compute_taproot_signature(adaptor_secret)?)
                .compute_signed_tx()?
                .signed_tx()?;
            // signed and ready to broadcast
            Ok(tx.clone())
        } else {
            anyhow::bail!("Only the seller can complete the SwapTx.")
        }
    }

    /// if Bob finds a `SwapTx` on chain (or in mempool), we can (and should) extract Alice key for
    /// unlocking the seller's deposit and fund, which is as adaptive secret in the signature
    pub fn reveal(&self, swap_tx: &Transaction, p_tik: &mut KeyCtx) -> anyhow::Result<()> {
        let signature = swap_tx.key_spend_signature(0)?;
        // calculate the aggregated secret key as well.
        // in swapTx reveal2Other makes only sense, when Seller gives to Buyer the secret key for p_tik
        if self.role == ProtocolRole::Buyer {
            p_tik.set_peers_prv_key(self.fund_sig.reveal_adaptor_secret(signature)?)?;
            p_tik.aggregate_prv_key_shares()?;
            tracing::debug!("revealed p_tik aggregated secret key: {:?}", p_tik.peers_key_share()?.prv_key()?);
            // p_tik shall have the other sec key and the aggregated secret key.
            // TODO Bob can import now the aggregated key into his wallet. there is no risk that
            //  Alice may publish any transaction messing with it.
        }
        Ok(())
    }

    pub fn broadcast(&self, me: &BMPContext) -> anyhow::Result<Txid> {
        me.chain.transaction_broadcast(self.builder.signed_tx()?)
    }
}

#[derive(Default)]
pub struct DepositTx {
    pub builder: DepositTxBuilder,
    pub merkle_root: Option<TapNodeHash>,
    pub p_descriptor: Option<Descriptor<DefiniteDescriptorKey>>,
    pub q_descriptor: Option<Descriptor<DefiniteDescriptorKey>>,
}

impl DepositTx {
    pub fn new() -> Self { Self::default() }

    fn merged_psbt(&self) -> anyhow::Result<&Psbt> { Ok(self.builder.psbt()?) }

    fn txid(&self) -> anyhow::Result<Txid> { Ok(*self.builder.txid()?) }

    pub fn generate_part_tx(&mut self, ctx: &mut BMPContext) -> anyhow::Result<Psbt> {
        self.builder
            .set_trade_amount(ctx.seller_amount - ctx.buyer_amount)
            .set_buyers_security_deposit(ctx.buyer_amount)
            .set_sellers_security_deposit(ctx.buyer_amount)
            .set_trade_fee_receivers(ReceiverList::default())
            .set_fee_rate(FeeRate::from_sat_per_vb_u32(20)); // TODO: feerates shall come from pricenodes

        let psbt = if ctx.am_buyer() {
            self.builder
                .init_buyers_half_psbt(&mut *ctx.funds, &mut rand::rng())?
                .buyers_half_psbt()?
        } else {
            self.builder
                .init_sellers_half_psbt(&mut *ctx.funds, &mut rand::rng())?
                .sellers_half_psbt()?
        };
        Ok(psbt.clone())
    }

    pub fn build_and_merge_tx(
        &mut self,
        ctx: &mut BMPContext,
        other_psbt: Psbt,
        p_tik: &KeyCtx,
        q_tik: &KeyCtx,
        buyer_pub_key: XOnlyPublicKey,
        seller_pub_key: XOnlyPublicKey,
    ) -> anyhow::Result<()> {
        if ctx.am_buyer() {
            self.builder.set_sellers_half_psbt(other_psbt);
        } else {
            self.builder.set_buyers_half_psbt(other_psbt);
        }
        self.merkle_root = Some(script_paths::deposit_payout_merkle_root(&buyer_pub_key, &seller_pub_key)?);
        self.p_descriptor = Some(script_paths::deposit_payout_descriptor(
            &p_tik.aggregated_key()?.pub_key().to_public_key().into(), &buyer_pub_key, &seller_pub_key)?);
        self.q_descriptor = Some(script_paths::deposit_payout_descriptor(
            &q_tik.aggregated_key()?.pub_key().to_public_key().into(), &buyer_pub_key, &seller_pub_key)?);
        let merkle_root = self.merkle_root.as_ref();
        self.builder
            .set_buyer_payout_address(p_tik.with_taproot_tweak(merkle_root)?.p2tr_address(Network::Regtest))
            .set_seller_payout_address(q_tik.with_taproot_tweak(merkle_root)?.p2tr_address(Network::Regtest))
            .compute_unsigned_tx()?;
        Ok(())
    }

    fn sign(&mut self, ctx: &mut BMPContext) -> anyhow::Result<()> {
        if ctx.am_buyer() {
            self.builder.sign_buyer_inputs(&mut *ctx.funds)?;
        } else {
            self.builder.sign_seller_inputs(&mut *ctx.funds)?;
        }
        Ok(())
    }

    fn transfer_sig_and_broadcast(
        &mut self,
        ctx: &mut BMPContext,
        psbt_bob: Psbt, // bobs psbt should be same as mine but have bob's sig
    ) -> anyhow::Result<Txid> {
        self.builder.combine_psbts(psbt_bob)?;

        let tx = self.builder.signed_tx()?;
        // TODO both alice and bob will broadcast, is that a bug or a feature?
        let deposit_txid = ctx.chain.transaction_broadcast(&tx)?;
        dbg!("DepositTx txid: {:?}", &deposit_txid);
        Ok(deposit_txid)
    }

    fn _get_outpoint_for(self, script: &Script) -> anyhow::Result<OutPoint> {
        let tx = &self.merged_psbt()?.unsigned_tx;

        for (index, output) in tx.output.iter().enumerate() {
            if output.script_pubkey == *script {
                return Ok(OutPoint {
                    txid: self.txid()?,
                    vout: u32::try_from(index)?,
                });
            }
        }

        Err(anyhow::anyhow!("No matching output found for the provided script"))
    }
}
