//! BT5: the use case UC5, a trade with a custom payout. Alice sells Bitcoin worth 0.2 BTC to Bob.
//! Instead of the normal close, Alice and Bob agree on a custom payout: of the trade amount,
//! Alice gets 0.15 BTC and Bob gets 0.05 BTC. Each also gets his security deposit back, and each
//! pays his half of the fee.

use anyhow::Context as _;
use bdk_wallet::bitcoin::{Amount, Weight};
use protocol::params::MIN_CONFIRMATIONS;
use rpc::pb::musigrpc::ClosureKind;

use crate::setup::{self, FUNDING, Setup};
use crate::trade::{self, FEE_RATE, Payouts, Terms};

const TRADE_ID: &str = "bt5";
/// Alice sells 0.2 BTC. The security deposits are those of the plan: 0.01 BTC of Bob, the buyer,
/// and 0.015 BTC of Alice, the seller.
const TERMS: Terms = Terms {
    trade_amount: Amount::from_sat(20_000_000),
    buyers_security_deposit: Amount::from_sat(1_000_000),
    sellers_security_deposit: Amount::from_sat(1_500_000),
};
/// The parts of the trade amount that Alice and Bob get in the custom payout.
const ALICES_PART: Amount = Amount::from_sat(15_000_000);
const BOBS_PART: Amount = Amount::from_sat(5_000_000);
/// The weight for which a custom payout transaction pays its fee, if both payout addresses are
/// Taproot addresses (`docs/specifications/trade/transactions.md`).
const CUSTOM_PAYOUT_WEIGHT: Weight = Weight::from_wu(1182);

#[tokio::test(flavor = "multi_thread")]
async fn bt5_alice_and_bob_agree_on_a_custom_payout() -> anyhow::Result<()> {
    assert_eq!(ALICES_PART + BOBS_PART, TERMS.trade_amount);
    // Precondition: both wallets hold 1 BTC.
    let mut setup = Setup::new().await?;

    // UC5: the trade. Alice is the seller and Bob is the buyer. They set up the trade, and Bob
    // publishes the deposit transaction.
    let deposit_tx = trade::set_up_and_confirm_deposit(&mut setup, TRADE_ID, &TERMS).await?;
    let (bobs_payout, _) = trade::payout_output(&deposit_tx, TERMS.buyers_payout())?;
    let (alices_payout, _) = trade::payout_output(&deposit_tx, TERMS.sellers_payout())?;
    // In this trade no trader gets the key of a payout. Each trader paid his part of the deposit
    // and a part of the fee of the deposit transaction. The parts of the fee are calculated from
    // the chain.
    let (alice, bob) = (&mut setup.alice, &mut setup.bob);
    let deposit_fees =
        trade::deposit_fees(alice, bob, &deposit_tx, &TERMS, Payouts::NotOwned).await?;
    setup::assert_small_fee(deposit_fees.sellers, "the deposit fee of Alice");
    setup::assert_small_fee(deposit_fees.buyers, "the deposit fee of Bob");

    // UC5: the custom payout, at a fee rate of 10 sat per vbyte. Before the fee, Alice gets her
    // part of the trade amount and her security deposit, and Bob gets the rest. Each trader pays
    // half of the fee; Bob pays 1 sat more if the fee is odd.
    let fee = FEE_RATE
        .fee_wu(CUSTOM_PAYOUT_WEIGHT)
        .context("the fee is too large")?;
    let alices_amount = ALICES_PART + TERMS.sellers_security_deposit - fee / 2;
    let bobs_amount = BOBS_PART + TERMS.buyers_security_deposit - (fee - fee / 2);

    // Both sign the same transaction, and their apps report the amounts of its outputs.
    let sellers_amount = ALICES_PART + TERMS.sellers_security_deposit;
    let (seller, buyer) = (&mut setup.alice.musig, &mut setup.bob.musig);
    let alices_psbt = trade::sign_custom_payout(seller, TRADE_ID, sellers_amount, FEE_RATE).await?;
    let bobs_psbt = trade::sign_custom_payout(buyer, TRADE_ID, sellers_amount, FEE_RATE).await?;
    for psbt in [&alices_psbt, &bobs_psbt] {
        assert_eq!(psbt.tx_id, alices_psbt.tx_id);
        assert_eq!(
            psbt.sellers_payout_amount_including_fee,
            alices_amount.to_sat()
        );
        assert_eq!(
            psbt.buyers_payout_amount_including_fee,
            bobs_amount.to_sat()
        );
    }
    // Each completes the transaction with the signatures of the other one, and his app publishes
    // it.
    let alices_tx = trade::finalise_custom_payout(seller, TRADE_ID, &bobs_psbt).await?;
    let bobs_tx = trade::finalise_custom_payout(buyer, TRADE_ID, &alices_psbt).await?;
    assert_eq!(alices_tx.compute_txid().to_string(), alices_psbt.tx_id);
    assert_eq!(bobs_tx.compute_txid(), alices_tx.compute_txid());

    // The transaction is confirmed, and both apps report the trade as closed with a custom
    // payout.
    let custom_payout_tx = setup.mine(&[alices_tx.compute_txid()]).await?.remove(0);
    for _ in 1..MIN_CONFIRMATIONS {
        setup.mine_block().await?;
    }
    for user in [&mut setup.alice, &mut setup.bob] {
        trade::wait_for_closure(user, TRADE_ID, ClosureKind::Custom).await?;
    }

    // Postcondition: the right amounts are on the chain. The custom payout transaction of the
    // chain spends the two payout outputs of the deposit transaction. Its first output pays the
    // amount of Bob and its second output pays the amount of Alice.
    let spent: Vec<_> = custom_payout_tx
        .input
        .iter()
        .map(|input| input.previous_output)
        .collect();
    assert_eq!(spent, [bobs_payout, alices_payout]);
    let paid: Vec<_> = custom_payout_tx
        .output
        .iter()
        .map(|output| output.value)
        .collect();
    assert_eq!(paid, [bobs_amount, alices_amount]);
    // Each output is a coin of the wallet of its trader.
    assert_eq!(
        trade::gain_by(&mut setup.alice, &custom_payout_tx).await?,
        alices_amount
    );
    assert_eq!(
        trade::gain_by(&mut setup.bob, &custom_payout_tx).await?,
        bobs_amount
    );
    // The fee is the fee rate of 10 sat per vbyte for the weight of the specification. The
    // transaction of the chain is not heavier, so it pays at least this rate.
    assert_eq!(setup::fee(&setup.bob.node, &custom_payout_tx)?, fee);
    assert!(
        custom_payout_tx.weight() <= CUSTOM_PAYOUT_WEIGHT,
        "the transaction has {}",
        custom_payout_tx.weight()
    );

    // Postcondition: Alice has 0.95 BTC and Bob has 1.05 BTC, minus the fees.
    let alices_balance = FUNDING - BOBS_PART - deposit_fees.sellers - fee / 2;
    let bobs_balance = FUNDING + BOBS_PART - deposit_fees.buyers - (fee - fee / 2);
    assert_eq!(FUNDING - BOBS_PART, Amount::from_sat(95_000_000));
    assert_eq!(FUNDING + BOBS_PART, Amount::from_sat(105_000_000));
    setup.alice.wait_for_balance(alices_balance).await?;
    setup.bob.wait_for_balance(bobs_balance).await?;
    Ok(())
}
