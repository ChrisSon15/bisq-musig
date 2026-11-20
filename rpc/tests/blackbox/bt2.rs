//! BT2: the use case UC2, a happy trade. Alice sells Bitcoin worth 0.1 BTC to Bob, and then both
//! close the app.

use anyhow::Context as _;
use bdk_wallet::bitcoin::Amount;
use rpc::pb::musigrpc::ClosureKind;

use crate::setup::{self, Coins, FUNDING, Setup};
use crate::trade::{self, Terms};

const TRADE_ID: &str = "bt2";
/// Alice sells 0.1 BTC. The plan does not name the security deposits, so the test chooses them.
const TERMS: Terms = Terms {
    trade_amount: Amount::from_sat(10_000_000),
    buyers_security_deposit: Amount::from_sat(1_000_000),
    sellers_security_deposit: Amount::from_sat(1_500_000),
};
/// A fee of the trade is small compared to the trade amount. The apps choose the fees, so the
/// test knows only a limit for them.
const FEE_LIMIT: Amount = Amount::from_sat(10_000);

fn assert_small_fee(fee: Amount, what: &str) {
    assert!(
        Amount::ZERO < fee && fee <= FEE_LIMIT,
        "{what} of {fee} is positive and small"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bt2_alice_sells_bitcoin_to_bob() -> anyhow::Result<()> {
    // Precondition: both wallets hold 1 BTC.
    let mut setup = Setup::new().await?;

    // UC2: the trade. Alice is the seller and Bob is the buyer. They set up the trade, Bob
    // publishes the deposit transaction, and after its confirmation they close the trade
    // cooperatively.
    let deposit = trade::set_up(&mut setup.alice, &mut setup.bob, TRADE_ID, &TERMS).await?;
    let deposit_txid = deposit.txid;
    let confirmations =
        trade::publish_deposit(&mut setup.alice, &mut setup.bob, TRADE_ID, deposit).await?;
    let deposit_tx = setup.mine(&[deposit_txid]).await?.remove(0);
    setup.mine_block().await?;
    setup.mine_block().await?;
    confirmations.wait_until_confirmed().await?;
    trade::close_cooperatively(&mut setup.alice, &mut setup.bob, TRADE_ID).await?;
    for user in [&mut setup.alice, &mut setup.bob] {
        let closure = trade::closure(user, TRADE_ID).await?;
        assert_eq!(closure, Some(ClosureKind::Cooperative));
    }

    // Each trader paid his part of the deposit and a part of the fee of the deposit
    // transaction. The parts are calculated from the chain.
    let sellers_part = TERMS.trade_amount + TERMS.sellers_security_deposit;
    let alices_deposit_fee = setup.alice.paid_into(&deposit_tx).await? - sellers_part;
    let bobs_deposit_fee = setup.bob.paid_into(&deposit_tx).await? - TERMS.buyers_security_deposit;
    assert_small_fee(alices_deposit_fee, "the deposit fee of Alice");
    assert_small_fee(bobs_deposit_fee, "the deposit fee of Bob");
    assert_eq!(
        alices_deposit_fee + bobs_deposit_fee,
        setup::fee(&setup.bob.node, &deposit_tx)?,
        "the two traders pay the fee of the deposit transaction"
    );

    // After the trade each trader owns his payout in an output of the deposit transaction. Its
    // key is an imported key of his wallet, not a key of the seed.
    let alice_after_trade = FUNDING - TERMS.trade_amount - alices_deposit_fee;
    let bob_after_trade = FUNDING + TERMS.trade_amount - bobs_deposit_fee;
    setup.alice.wait_for_balance(alice_after_trade).await?;
    setup.bob.wait_for_balance(bob_after_trade).await?;
    assert_eq!(
        setup.alice.coins().await?.imported,
        [TERMS.sellers_security_deposit]
    );
    assert_eq!(
        setup.bob.coins().await?.imported,
        [TERMS.trade_amount + TERMS.buyers_security_deposit]
    );

    // UC2: both close the app. Each app sends a drain transaction, which moves the payout to the
    // seed wallet. The network confirms the two transactions while the apps are closed.
    let alices_drain = setup.alice.close_app().await?;
    let bobs_drain = setup.bob.close_app().await?;
    let alices_drain = alices_drain.context("the app of Alice sent no drain transaction")?;
    let bobs_drain = bobs_drain.context("the app of Bob sent no drain transaction")?;
    let drain_txs = setup.mine(&[alices_drain, bobs_drain]).await?;
    let alices_drain_fee = setup::fee(&setup.bob.node, &drain_txs[0])?;
    let bobs_drain_fee = setup::fee(&setup.bob.node, &drain_txs[1])?;
    assert_small_fee(alices_drain_fee, "the drain fee of Alice");
    assert_small_fee(bobs_drain_fee, "the drain fee of Bob");

    // The postconditions are checked in the apps, so both users start the app again.
    setup.alice.open_app().await?;
    setup.bob.open_app().await?;

    // Postcondition: Bob has 0.1 BTC more and Alice has 0.1 BTC less, minus the fees.
    let alices_balance = alice_after_trade - alices_drain_fee;
    let bobs_balance = bob_after_trade - bobs_drain_fee;
    setup.alice.wait_for_balance(alices_balance).await?;
    setup.bob.wait_for_balance(bobs_balance).await?;

    // Postcondition: all coins of Alice and of Bob are coins of the seed wallet. No coin of an
    // imported key is left.
    for (user, balance) in [
        (&mut setup.alice, alices_balance),
        (&mut setup.bob, bobs_balance),
    ] {
        let Coins { seed, imported } = user.coins().await?;
        assert_eq!(imported, [], "no coin of an imported key is left");
        assert_eq!(seed.into_iter().sum::<Amount>(), balance);
    }
    Ok(())
}
