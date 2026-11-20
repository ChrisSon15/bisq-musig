//! The parts of a trade that only the blackbox tests need: the trade of Alice and Bob up to the
//! confirmed deposit, the steps of a custom payout, and the checks of the results on the chain
//! and in the wallets. The other steps of the trade protocol are in [`crate::common::trade`].

use anyhow::Context as _;
use bdk_wallet::bitcoin::key::{Keypair, Secp256k1, TapTweak as _};
use bdk_wallet::bitcoin::secp256k1::Message;
use bdk_wallet::bitcoin::{
    Amount, FeeRate, OutPoint, SignedAmount, Transaction, TxOut, XOnlyPublicKey, consensus,
};
use futures_util::StreamExt as _;
use protocol::params::MIN_CONFIRMATIONS;
use rpc::pb::musigrpc::musig_client::MusigClient;
use rpc::pb::musigrpc::{
    ClosureKind, CustomCloseTradeRequest, CustomPayoutPsbt, CustomPayoutPsbtRequest, TradeStateKind,
};
use tonic::transport::Channel;
use wallet::bmp_wallet::ImportedKey;

use crate::common::musigd::{WAIT, wait_for};
use crate::common::trade::{self, DepositConfirmations};
pub use crate::common::trade::{
    FEE_RATE, Terms, close_cooperatively, close_request, release, start_payment,
};
use crate::setup::{self, Setup, User};

impl Terms {
    /// The amount of the output of the deposit transaction that the buyer owns after a
    /// cooperative close.
    pub fn buyers_payout(&self) -> Amount {
        self.trade_amount + self.buyers_security_deposit
    }

    /// The amount of the output of the deposit transaction that the seller owns after a
    /// cooperative close.
    pub const fn sellers_payout(&self) -> Amount {
        self.sellers_security_deposit
    }
}

/// The parts of the fee of the deposit transaction that the two traders paid.
pub struct DepositFees {
    pub sellers: Amount,
    pub buyers: Amount,
}

/// Whether the wallets of the traders count the two payout outputs of the deposit transaction as
/// their coins. A wallet counts the payout of its user from the moment at which it holds the key
/// of the output.
#[derive(Clone, Copy)]
pub enum Payouts {
    /// Each trader holds the key of his payout: the trade is closed cooperatively.
    Owned,
    /// No trader holds the key of his payout: no key share was handed over.
    NotOwned,
}

/// Alice and Bob set up a trade in which Alice is the seller and Bob is the buyer. Bob publishes
/// the deposit transaction, and blocks are mined until it counts as confirmed. Returns the
/// deposit transaction as the chain contains it.
pub async fn set_up_and_confirm_deposit(
    setup: &mut Setup,
    trade_id: &str,
    terms: &Terms,
) -> anyhow::Result<Transaction> {
    // The receiver of the redirect transactions is part of the contract. An address of a node
    // stands for it.
    let redirect_receiver = setup.alice.node.new_address()?;
    let (seller, buyer) = (&mut setup.alice.musig, &mut setup.bob.musig);
    let deposit = trade::set_up(seller, buyer, trade_id, terms, redirect_receiver).await?;
    let txid = deposit.txid;
    let confirmations = trade::publish_deposit(seller, buyer, trade_id, deposit).await?;
    let deposit_tx = setup.mine(&[txid]).await?.remove(0);
    for _ in 1..MIN_CONFIRMATIONS {
        setup.mine_block().await?;
    }
    wait_until_confirmed(confirmations).await?;
    Ok(deposit_tx)
}

/// Calculates from the chain which part of the fee of the deposit transaction each trader paid.
/// `payouts` tells whether the wallets count the payout outputs at this moment. Without a fee,
/// the deposit transaction would reduce the coins of a wallet by what the trader paid in, less
/// his payout if the wallet counts it: the seller paid the trade amount and his security deposit,
/// and the buyer paid his security deposit. What a trader lost beyond that is his part of the
/// fee. Fails if the two parts are not the fee of the transaction.
pub async fn deposit_fees(
    seller: &mut User,
    buyer: &mut User,
    deposit_tx: &Transaction,
    terms: &Terms,
    payouts: Payouts,
) -> anyhow::Result<DepositFees> {
    let mut sellers_stake = (terms.trade_amount + terms.sellers_security_deposit).to_signed()?;
    let mut buyers_stake = terms.buyers_security_deposit.to_signed()?;
    if matches!(payouts, Payouts::Owned) {
        sellers_stake -= terms.sellers_payout().to_signed()?;
        buyers_stake -= terms.buyers_payout().to_signed()?;
    }
    let sellers = seller.loss_by(deposit_tx).await? - sellers_stake;
    let buyers = buyer.loss_by(deposit_tx).await? - buyers_stake;
    let fees = DepositFees {
        sellers: sellers
            .to_unsigned()
            .context("the seller lost less than he paid into the deposit")?,
        buyers: buyers
            .to_unsigned()
            .context("the buyer lost less than he paid into the deposit")?,
    };
    let fee = setup::fee(&buyer.node, deposit_tx)?;
    anyhow::ensure!(
        fees.sellers + fees.buyers == fee,
        "the traders paid {} and {} for a deposit transaction with a fee of {fee}",
        fees.sellers,
        fees.buyers
    );
    Ok(fees)
}

/// Waits until both apps have reported that the deposit transaction counts as confirmed. An app
/// reports the confirmations up to [`MIN_CONFIRMATIONS`] and then ends its reports.
async fn wait_until_confirmed(mut confirmations: DepositConfirmations) -> anyhow::Result<()> {
    for (trader, reports) in [
        ("seller", &mut confirmations.sellers),
        ("buyer", &mut confirmations.buyers),
    ] {
        let mut confirmed = 0;
        while let Some(report) = tokio::time::timeout(WAIT, reports.next())
            .await
            .with_context(|| format!("the app of the {trader} reports no confirmation"))?
        {
            confirmed = report?.num_confirmations;
        }
        anyhow::ensure!(
            confirmed == u32::from(MIN_CONFIRMATIONS),
            "the reports of the {trader} end at {confirmed} confirmations"
        );
    }
    Ok(())
}

/// How the trade was closed, as the app of `user` reports it, or `None` if the trade is not
/// closed.
pub async fn closure(user: &mut User, trade_id: &str) -> anyhow::Result<Option<ClosureKind>> {
    let state = trade::trade_state(&mut user.musig, trade_id).await?;
    Ok((state.state() == TradeStateKind::TradeClosed).then(|| state.closure_type()))
}

/// Waits until the app of `user` reports that the trade is closed in the way `closure`.
pub async fn wait_for_closure(
    user: &mut User,
    trade_id: &str,
    closure: ClosureKind,
) -> anyhow::Result<()> {
    let what = format!("the trade {trade_id} is closed as {closure:?}");
    wait_for(&what, async || {
        Ok((self::closure(user, trade_id).await? == Some(closure)).then_some(()))
    })
    .await
}

/// The output of the deposit transaction that pays `amount`, which is the amount of a payout,
/// with its position on the chain. Fails unless exactly one output pays this amount.
pub fn payout_output(
    deposit_tx: &Transaction,
    amount: Amount,
) -> anyhow::Result<(OutPoint, &TxOut)> {
    let mut outputs = (0..)
        .zip(&deposit_tx.output)
        .filter(|(_, output)| output.value == amount);
    let output = outputs.next();
    anyhow::ensure!(
        outputs.next().is_none(),
        "more than one output of the deposit transaction pays {amount}"
    );
    let (vout, output) =
        output.with_context(|| format!("no output of the deposit transaction pays {amount}"))?;
    Ok((OutPoint::new(deposit_tx.compute_txid(), vout), output))
}

/// The amount that the transaction `tx` of the chain added to the coins of the wallet of `user`.
/// Fails if the transaction reduced them.
pub async fn gain_by(user: &mut User, tx: &Transaction) -> anyhow::Result<Amount> {
    let gain: SignedAmount = -user.loss_by(tx).await?;
    gain.to_unsigned()
        .context("the transaction reduced the coins of the wallet")
}

/// Step 1 of a custom payout: the trader signs the custom payout transaction that gives the
/// seller `sellers_amount` before the fee. Returns the transaction with the signatures of the
/// trader, its ID and the amounts of its two outputs.
pub async fn sign_custom_payout(
    musig: &mut MusigClient<Channel>,
    trade_id: &str,
    sellers_amount: Amount,
    fee_rate: FeeRate,
) -> anyhow::Result<CustomPayoutPsbt> {
    let request = CustomPayoutPsbtRequest {
        trade_id: trade_id.to_owned(),
        sellers_payout_amount_excluding_fee: sellers_amount.to_sat(),
        fee_rate: fee_rate.to_sat_per_kwu(),
    };
    Ok(musig.sign_custom_payout_tx(request).await?.into_inner())
}

/// Step 2 of a custom payout: the trader completes the transaction with the signatures of the
/// peer, and his app publishes it. Returns the complete transaction.
pub async fn finalise_custom_payout(
    musig: &mut MusigClient<Channel>,
    trade_id: &str,
    peers_psbt: &CustomPayoutPsbt,
) -> anyhow::Result<Transaction> {
    let request = CustomCloseTradeRequest {
        trade_id: trade_id.to_owned(),
        peers_custom_payout_psbt: peers_psbt.psbt.clone(),
    };
    let response = musig.custom_close_trade(request).await?.into_inner();
    Ok(consensus::deserialize(&response.custom_payout_tx)?)
}

/// Proves that `key` can spend the Taproot output `output` of the chain. A message of 32 random
/// bytes is signed with the key, and the signature is verified with the public key that the
/// script of the output contains. Fails if the signature is not valid for this public key.
pub fn prove_key_of_output(key: &ImportedKey, output: &TxOut) -> anyhow::Result<()> {
    let secp = Secp256k1::new();
    // The public key from the chain. The script of a Taproot output is the version number 1 and
    // the 32 bytes of the public key of the output.
    let script = &output.script_pubkey;
    anyhow::ensure!(script.is_p2tr(), "the output is not a Taproot output");
    let output_key = XOnlyPublicKey::from_slice(&script.as_bytes()[2..])?;

    // The secret key of the output is the imported key, tweaked with the scripts of the output
    // (BIP341).
    let keypair = Keypair::from_seckey_slice(&secp, &key.secret().to_bytes()[..])?;
    let signing_key = keypair.tap_tweak(&secp, key.merkle_root()).to_keypair();
    let message = Message::from_digest(rand::random());
    let signature = secp.sign_schnorr_no_aux_rand(&message, &signing_key);
    secp.verify_schnorr(&signature, &message, &output_key)
        .context("the signature of the key is not valid for the public key of the output")
}
