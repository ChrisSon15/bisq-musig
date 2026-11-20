//! BT8: the use case UC8, a trade that ends with a penalty. Alice sells Bitcoin worth 0.2 BTC to
//! Bob, and they finish the trade successfully. Afterwards Bob publishes his warning transaction,
//! to take all funds of the trade with a claim. The app of Alice answers with her penalty
//! transaction, which pays all funds of the trade to Alice.
//!
//! The app of Bob refuses to publish the warning transaction of a closed trade. Bob gets around
//! this with an old copy of his data: he makes a backup of his wallet directory before the trade
//! is closed, and later starts his app on this backup, in which the trade is still open.

use anyhow::Context as _;
use bdk_wallet::bitcoin::{Amount, OutPoint, Sequence, Weight};
use protocol::params::MIN_CONFIRMATIONS;
use rpc::pb::musigrpc::{ClosureKind, TradeTxKind};

use crate::setup::{self, CLAIM_LOCK_TIME, FUNDING, Setup};
use crate::trade::{self, FEE_RATE, Payouts, Terms};

const TRADE_ID: &str = "bt8";
/// Alice sells 0.2 BTC. The security deposits are those of the plan: 0.01 BTC of Bob, the buyer,
/// and 0.015 BTC of Alice, the seller.
const TERMS: Terms = Terms {
    trade_amount: Amount::from_sat(20_000_000),
    buyers_security_deposit: Amount::from_sat(1_000_000),
    sellers_security_deposit: Amount::from_sat(1_500_000),
};
/// The weight for which a penalty transaction pays its fee
/// (`docs/specifications/trade/transactions.md`).
const PENALTY_WEIGHT: Weight = Weight::from_wu(444);

#[tokio::test(flavor = "multi_thread")]
async fn bt8_bob_warns_after_the_trade_and_alice_takes_the_penalty() -> anyhow::Result<()> {
    // Precondition: both wallets hold 1 BTC.
    let mut setup = Setup::with_short_lock_times().await?;

    // UC8: the trade. Alice is the seller and Bob is the buyer. They set up the trade, and Bob
    // publishes the deposit transaction. The fee rate of the trade is 10 sat per vbyte.
    let deposit_tx = trade::set_up_and_confirm_deposit(&mut setup, TRADE_ID, &TERMS).await?;
    // No trader holds the key of a payout yet. Each trader paid his part of the deposit and a
    // part of the fee of the deposit transaction. The parts of the fee are calculated from the
    // chain.
    let (alice, bob) = (&mut setup.alice, &mut setup.bob);
    let deposit_fees =
        trade::deposit_fees(alice, bob, &deposit_tx, &TERMS, Payouts::NotOwned).await?;
    setup::assert_small_fee(deposit_fees.sellers, "the deposit fee of Alice");
    setup::assert_small_fee(deposit_fees.buyers, "the deposit fee of Bob");

    // Bob prepares his attempt: he makes a backup of his wallet directory while the trade is
    // open. He ends his app for it and starts it again. The close of the app moves nothing,
    // because no trade is closed.
    let drain = setup.bob.close_app().await?;
    assert_eq!(drain, None, "nothing to move before the trade is closed");
    let bobs_backup = setup.bob.back_up_wallet()?;
    setup.bob.open_app().await?;

    // UC8: Alice and Bob finish the trade successfully. They close it cooperatively: Bob has
    // 0.2 BTC more and Alice has 0.2 BTC less, minus the fees.
    let (seller, buyer) = (&mut setup.alice.musig, &mut setup.bob.musig);
    trade::close_cooperatively(seller, buyer, TRADE_ID).await?;
    for user in [&mut setup.alice, &mut setup.bob] {
        let closure = trade::closure(user, TRADE_ID).await?;
        assert_eq!(closure, Some(ClosureKind::Cooperative));
    }
    let alice_after_trade = FUNDING - TERMS.trade_amount - deposit_fees.sellers;
    let bob_after_trade = FUNDING + TERMS.trade_amount - deposit_fees.buyers;
    setup.alice.wait_for_balance(alice_after_trade).await?;
    setup.bob.wait_for_balance(bob_after_trade).await?;

    // The app of Bob refuses to publish the warning transaction of the closed trade.
    let refused = trade::publish_warning(&mut setup.bob.musig, TRADE_ID).await;
    assert!(refused.is_err(), "the trade is closed");

    // UC8: now Bob issues the warning transaction. He ends his app, puts the backup in the place
    // of his wallet directory, and starts the app. In the backup the trade is open, so this app
    // publishes the warning transaction when the warning lock time has passed.
    setup.bob.stop_app();
    setup.bob.restore_wallet(&bobs_backup)?;
    setup.bob.open_app().await?;
    let mut alices_events = trade::trade_events(&mut setup.alice.musig, TRADE_ID).await?;
    // Postcondition: Bob sent the warning transaction.
    let warning = trade::bob_publishes_warning(&mut setup, TRADE_ID, &deposit_tx, &TERMS).await?;
    let [escrow, bobs_anchor] = &warning.tx.output[..] else {
        panic!("the warning transaction has two outputs");
    };

    // The app of Alice sends the penalty transaction on its own when it sees the confirmed
    // warning transaction of Bob; no request asks for it. Alice can sign it alone, because she
    // holds the whole key of the escrow since the trade was closed.
    let penalty_txid = trade::wait_for_sent(&mut alices_events, TradeTxKind::PenaltyTx).await?;
    let penalty_tx = setup.mine(&[penalty_txid]).await?.remove(0);
    let penalty_height = setup.height()?;
    for _ in 1..MIN_CONFIRMATIONS {
        setup.mine_block().await?;
    }
    // Both apps report the trade as closed by the penalty of the seller.
    for user in [&mut setup.alice, &mut setup.bob] {
        trade::wait_for_closure(user, TRADE_ID, ClosureKind::SellersPenaltyTx).await?;
    }

    // Postcondition: Alice sent the penalty transaction. The transaction of the chain spends the
    // escrow output of the warning transaction with one signature and without a lock time, and
    // pays everything, less its fee, to the wallet of Alice.
    let [penalty_input] = &penalty_tx.input[..] else {
        panic!(
            "the penalty transaction has {} inputs",
            penalty_tx.input.len()
        );
    };
    let escrow_output = OutPoint::new(warning.tx.compute_txid(), 0);
    assert_eq!(penalty_input.previous_output, escrow_output);
    assert_eq!(penalty_input.sequence, Sequence::ENABLE_RBF_NO_LOCKTIME);
    assert_eq!(penalty_input.witness.len(), 1, "a spend with the key path");
    assert_eq!(penalty_tx.output.len(), 1);
    // The fee is the fee rate of 10 sat per vbyte for the weight of the specification.
    let penalty_fee = setup::fee(&setup.bob.node, &penalty_tx)?;
    let expected_fee = FEE_RATE.fee_wu(PENALTY_WEIGHT);
    assert_eq!(Some(penalty_fee), expected_fee);
    assert_eq!(
        trade::gain_by(&mut setup.alice, &penalty_tx).await?,
        escrow.value - penalty_fee
    );
    // It was confirmed long before Bob could claim the escrow.
    assert!(penalty_height < warning.height + u64::from(CLAIM_LOCK_TIME));

    // Postcondition: Bob has 0.99 BTC, minus the fees. He lost his security deposit and what he
    // bought. He got the anchor of his warning transaction, which is 330 sat.
    let bob_without_deposit = FUNDING - TERMS.buyers_security_deposit;
    assert_eq!(bob_without_deposit, Amount::from_sat(99_000_000));
    let bobs_balance = bob_without_deposit - deposit_fees.buyers + bobs_anchor.value;
    setup.bob.wait_for_balance(bobs_balance).await?;

    // Postcondition: Alice has 1.01 BTC, minus the fees. She holds all funds of the trade: her
    // 1 BTC and the security deposit of Bob. The fees are her part of the deposit fee, the fee
    // and the anchor of the warning transaction of Bob, and the fee of her penalty transaction.
    let alice_with_all_funds = FUNDING + TERMS.buyers_security_deposit;
    assert_eq!(alice_with_all_funds, Amount::from_sat(101_000_000));
    let alices_fees = deposit_fees.sellers + warning.fee + bobs_anchor.value + penalty_fee;
    let alices_balance = alice_with_all_funds
        .checked_sub(alices_fees)
        .context("the fees are larger than the funds")?;
    setup.alice.wait_for_balance(alices_balance).await?;
    Ok(())
}
