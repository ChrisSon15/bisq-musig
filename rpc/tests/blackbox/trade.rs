//! The parts of a trade that only the blackbox tests need: the trade of Alice and Bob up to the
//! confirmed deposit, and the checks of its results on the chain and in the wallets. The steps of
//! the trade protocol are in [`crate::common::trade`].

use anyhow::Context as _;
use bdk_wallet::bitcoin::key::{Keypair, Secp256k1, TapTweak as _};
use bdk_wallet::bitcoin::secp256k1::Message;
use bdk_wallet::bitcoin::{Amount, Transaction, TxOut, XOnlyPublicKey};
use futures_util::StreamExt as _;
use protocol::params::MIN_CONFIRMATIONS;
use rpc::pb::musigrpc::{ClosureKind, TradeStateKind};
use wallet::bmp_wallet::ImportedKey;

use crate::common::musigd::WAIT;
use crate::common::trade::{self, DepositConfirmations};
pub use crate::common::trade::{Terms, close_cooperatively};
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
/// The trade must be closed cooperatively: each trader then owns his payout output of the
/// deposit transaction. Without a fee, the deposit transaction would reduce the coins of the
/// seller by the trade amount and raise the coins of the buyer by the trade amount. What a trader
/// lost beyond that is his part of the fee. Fails if the two parts are not the fee of the
/// transaction.
pub async fn deposit_fees(
    seller: &mut User,
    buyer: &mut User,
    deposit_tx: &Transaction,
    terms: &Terms,
) -> anyhow::Result<DepositFees> {
    let trade_amount = terms.trade_amount.to_signed()?;
    let sellers = seller.loss_by(deposit_tx).await? - trade_amount;
    let buyers = buyer.loss_by(deposit_tx).await? + trade_amount;
    let fees = DepositFees {
        sellers: sellers
            .to_unsigned()
            .context("the seller lost less than the trade amount")?,
        buyers: buyers
            .to_unsigned()
            .context("the buyer gained more than the trade amount")?,
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

/// The output of the deposit transaction that pays `amount`, which is the amount of a payout.
/// Fails unless exactly one output pays this amount.
pub fn payout_output(deposit_tx: &Transaction, amount: Amount) -> anyhow::Result<&TxOut> {
    let mut outputs = deposit_tx
        .output
        .iter()
        .filter(|output| output.value == amount);
    let output = outputs.next();
    anyhow::ensure!(
        outputs.next().is_none(),
        "more than one output of the deposit transaction pays {amount}"
    );
    output.with_context(|| format!("no output of the deposit transaction pays {amount}"))
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
