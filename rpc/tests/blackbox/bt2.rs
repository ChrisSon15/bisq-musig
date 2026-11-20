//! BT2: the use case UC2, a happy trade. Alice sells Bitcoin worth 0.1 BTC to Bob, and then both
//! close the app.

use anyhow::Context as _;
use bdk_wallet::bitcoin::Amount;
use rpc::pb::musigrpc::ClosureKind;

use crate::setup::{self, Coins, FUNDING, Setup};
use crate::trade::{self, Payouts, Terms};

const TRADE_ID: &str = "bt2";
/// Alice sells 0.1 BTC. The security deposits are those of the plan: 0.01 BTC of Bob, the buyer,
/// and 0.015 BTC of Alice, the seller.
const TERMS: Terms = Terms {
    trade_amount: Amount::from_sat(10_000_000),
    buyers_security_deposit: Amount::from_sat(1_000_000),
    sellers_security_deposit: Amount::from_sat(1_500_000),
};

#[tokio::test(flavor = "multi_thread")]
async fn bt2_alice_sells_bitcoin_to_bob() -> anyhow::Result<()> {
    // Precondition: both wallets hold 1 BTC.
    let mut setup = Setup::new().await?;

    // UC2: the trade. Alice is the seller and Bob is the buyer. They set up the trade, Bob
    // publishes the deposit transaction, and after its confirmation they close the trade
    // cooperatively.
    let deposit_tx = trade::set_up_and_confirm_deposit(&mut setup, TRADE_ID, &TERMS).await?;
    let (seller, buyer) = (&mut setup.alice.musig, &mut setup.bob.musig);
    trade::close_cooperatively(seller, buyer, TRADE_ID).await?;
    for user in [&mut setup.alice, &mut setup.bob] {
        let closure = trade::closure(user, TRADE_ID).await?;
        assert_eq!(closure, Some(ClosureKind::Cooperative));
    }

    // Each trader paid his part of the deposit and a part of the fee of the deposit
    // transaction. The parts of the fee are calculated from the chain.
    let (alice, bob) = (&mut setup.alice, &mut setup.bob);
    let deposit_fees = trade::deposit_fees(alice, bob, &deposit_tx, &TERMS, Payouts::Owned).await?;
    setup::assert_small_fee(deposit_fees.sellers, "the deposit fee of Alice");
    setup::assert_small_fee(deposit_fees.buyers, "the deposit fee of Bob");

    // After the trade each trader owns his payout in an output of the deposit transaction, and
    // his wallet counts it.
    let alice_after_trade = FUNDING - TERMS.trade_amount - deposit_fees.sellers;
    let bob_after_trade = FUNDING + TERMS.trade_amount - deposit_fees.buyers;
    setup.alice.wait_for_balance(alice_after_trade).await?;
    setup.bob.wait_for_balance(bob_after_trade).await?;

    // UC2: both close the app. Each app sends a drain transaction, which moves the payout to the
    // seed wallet, and then its `musigd` process ends in the regular way. The network confirms
    // the two transactions while the apps are closed.
    let alices_drain = setup.alice.close_app().await?;
    let bobs_drain = setup.bob.close_app().await?;
    let alices_drain = alices_drain.context("the app of Alice sent no drain transaction")?;
    let bobs_drain = bobs_drain.context("the app of Bob sent no drain transaction")?;
    let drain_txs = setup.mine(&[alices_drain, bobs_drain]).await?;
    let alices_drain_fee = setup::fee(&setup.bob.node, &drain_txs[0])?;
    let bobs_drain_fee = setup::fee(&setup.bob.node, &drain_txs[1])?;
    setup::assert_small_fee(alices_drain_fee, "the drain fee of Alice");
    setup::assert_small_fee(bobs_drain_fee, "the drain fee of Bob");

    // The postconditions are checked in the apps, so both users start the app again.
    setup.alice.open_app().await?;
    setup.bob.open_app().await?;

    // Postcondition: Bob has 0.1 BTC more and Alice has 0.1 BTC less, minus the fees.
    let alices_balance = alice_after_trade - alices_drain_fee;
    let bobs_balance = bob_after_trade - bobs_drain_fee;
    setup.alice.wait_for_balance(alices_balance).await?;
    setup.bob.wait_for_balance(bobs_balance).await?;

    // Postcondition: all coins of Alice and of Bob are coins of the seed wallet. No coin is left
    // in the wallet of an imported key. The wallet service does not show in which part of a
    // wallet a coin is, so both close the app again, and the test reads the wallets themselves.
    // This close has nothing to move.
    for (user, balance) in [
        (&mut setup.alice, alices_balance),
        (&mut setup.bob, bobs_balance),
    ] {
        let drain = user.close_app().await?;
        assert_eq!(drain, None, "the second close moves nothing");
        let Coins { seed, imported } = user.coins()?;
        assert_eq!(imported, [], "no coin of an imported key is left");
        assert_eq!(seed.into_iter().sum::<Amount>(), balance);
    }
    Ok(())
}
