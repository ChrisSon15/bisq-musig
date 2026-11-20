//! BT4: the use case UC4, a happy trade in which the seller uses the swap transaction. Alice
//! sells Bitcoin worth 0.1 BTC to Bob. Alice releases the payout of Bob, but Bob gets no message
//! from her. Alice closes the trade alone with her swap transaction. The app of Bob finds this
//! transaction on the chain and computes the key of his payout from its signature. Bob then moves
//! the payout to his main wallet.

use anyhow::Context as _;
use bdk_wallet::bitcoin::{Amount, Transaction, consensus};
use rpc::pb::musigrpc::{ClosureKind, TradeStateKind};

use crate::common::trade::trade_state;
use crate::setup::{self, Coins, FUNDING, Setup};
use crate::trade::{self, Payouts, Terms};

const TRADE_ID: &str = "bt4";
/// Alice sells 0.1 BTC. The security deposits are those of the plan: 0.01 BTC of Bob, the buyer,
/// and 0.015 BTC of Alice, the seller.
const TERMS: Terms = Terms {
    trade_amount: Amount::from_sat(10_000_000),
    buyers_security_deposit: Amount::from_sat(1_000_000),
    sellers_security_deposit: Amount::from_sat(1_500_000),
};

#[tokio::test(flavor = "multi_thread")]
async fn bt4_alice_closes_the_trade_with_the_swap_transaction() -> anyhow::Result<()> {
    // Precondition: both wallets hold 1 BTC.
    let mut setup = Setup::new().await?;

    // UC4: the trade. Alice is the seller and Bob is the buyer. They set up the trade, and Bob
    // publishes the deposit transaction.
    let deposit_tx = trade::set_up_and_confirm_deposit(&mut setup, TRADE_ID, &TERMS).await?;
    let (alices_payout, alices_output) = trade::payout_output(&deposit_tx, TERMS.sellers_payout())?;
    let (bobs_payout, bobs_output) = trade::payout_output(&deposit_tx, TERMS.buyers_payout())?;
    // No trader holds the key of a payout yet. Each trader paid his part of the deposit and a
    // part of the fee of the deposit transaction. The parts of the fee are calculated from the
    // chain.
    let (alice, bob) = (&mut setup.alice, &mut setup.bob);
    let deposit_fees =
        trade::deposit_fees(alice, bob, &deposit_tx, &TERMS, Payouts::NotOwned).await?;
    setup::assert_small_fee(deposit_fees.sellers, "the deposit fee of Alice");
    setup::assert_small_fee(deposit_fees.buyers, "the deposit fee of Bob");

    // Bob starts the payment, and Alice releases his payout. The message of Alice with her key
    // share for the payout of Bob (message F) does not reach Bob: the test does not pass it on.
    // From here on the app of Bob gets no data of Alice from the test.
    let (seller, buyer) = (&mut setup.alice.musig, &mut setup.bob.musig);
    trade::start_payment(seller, buyer, TRADE_ID).await?;
    let undelivered_message_f = trade::release(seller, TRADE_ID).await?;
    assert!(undelivered_message_f.is_some(), "Alice released");
    // Bob does not own his payout: his trade is open, and his wallet does not count the payout.
    let bobs_state = trade_state(&mut setup.bob.musig, TRADE_ID).await?;
    assert_eq!(bobs_state.state(), TradeStateKind::BuyerReadyToRelease);
    let bob_without_payout = FUNDING - TERMS.buyers_security_deposit - deposit_fees.buyers;
    setup.bob.wait_for_balance(bob_without_payout).await?;

    // UC4: Alice uses the swap transaction. Her app publishes it, and her trade is closed.
    let closed = setup
        .alice
        .musig
        .close_trade(trade::close_request(TRADE_ID, None))
        .await?
        .into_inner();
    let swap_tx = closed
        .swap_tx
        .context("the app of Alice returns no swap transaction")?;
    let swap_tx: Transaction = consensus::deserialize(&swap_tx)?;
    let closure = trade::closure(&mut setup.alice, TRADE_ID).await?;
    assert_eq!(closure, Some(ClosureKind::Forced));

    // The swap transaction on the chain spends the payout output of Alice with one signature,
    // and pays her security deposit, less its fee, to her wallet.
    let swap_tx = setup.mine(&[swap_tx.compute_txid()]).await?.remove(0);
    let [swap_input] = &swap_tx.input[..] else {
        panic!("the swap transaction has {} inputs", swap_tx.input.len());
    };
    assert_eq!(swap_input.previous_output, alices_payout);
    assert_eq!(swap_input.witness.len(), 1, "a spend with the key path");
    let swap_fee = setup::fee(&setup.bob.node, &swap_tx)?;
    setup::assert_small_fee(swap_fee, "the fee of the swap transaction");
    assert_eq!(
        trade::gain_by(&mut setup.alice, &swap_tx).await?,
        TERMS.sellers_security_deposit - swap_fee
    );

    // The app of Bob reaches the chain only through the peer-to-peer port of his node, with
    // compact block filters. It finds the swap transaction there and closes the trade of Bob.
    // The key of the payout of Bob consists of a key share of Bob and a key share of Alice. The
    // app of Bob got the key share of Alice from nobody, so it can only have computed it from
    // the signature of the swap transaction and from the adaptor signature that it holds since
    // the setup. The closure kind "forced" is the report of the app about this.
    trade::wait_for_closure(&mut setup.bob, TRADE_ID, ClosureKind::Forced).await?;

    // Now the wallet of Bob counts the payout. Bob has 0.1 BTC more and Alice has 0.1 BTC less,
    // minus the fees.
    let bob_after_trade = FUNDING + TERMS.trade_amount - deposit_fees.buyers;
    let alices_balance = FUNDING - TERMS.trade_amount - deposit_fees.sellers - swap_fee;
    setup.bob.wait_for_balance(bob_after_trade).await?;
    setup.alice.wait_for_balance(alices_balance).await?;

    // Bob sends his trade output to his main wallet: he closes the app, which sends a drain
    // transaction. The transaction spends the payout output of Bob with one signature. The
    // network accepts it, so the key that the app of Bob computed is the key of the output.
    let bobs_drain = setup.bob.close_app().await?;
    let bobs_drain = bobs_drain.context("the app of Bob sent no drain transaction")?;
    let drain_tx = setup.mine(&[bobs_drain]).await?.remove(0);
    let [drain_input] = &drain_tx.input[..] else {
        panic!("the drain transaction has {} inputs", drain_tx.input.len());
    };
    assert_eq!(drain_input.previous_output, bobs_payout);
    assert_eq!(drain_input.witness.len(), 1, "a spend with the key path");
    let drain_fee = setup::fee(&setup.alice.node, &drain_tx)?;
    setup::assert_small_fee(drain_fee, "the drain fee of Bob");

    // Postcondition: Bob has 0.1 BTC more and Alice has 0.1 BTC less, minus the fees. Bob starts
    // the app again for this check.
    setup.bob.open_app().await?;
    let bobs_balance = bob_after_trade - drain_fee;
    setup.bob.wait_for_balance(bobs_balance).await?;
    setup.alice.wait_for_balance(alices_balance).await?;

    // The other postconditions concern the internal structure of the wallet of Bob, which the
    // wallet service does not show. Bob closes the app again, which has nothing to move, and the
    // test reads the wallet itself.
    let drain = setup.bob.close_app().await?;
    assert_eq!(drain, None, "the second close moves nothing");
    // All coins of Bob are coins of his main wallet.
    let Coins { seed, imported } = setup.bob.coins()?;
    assert_eq!(imported, [], "no coin of an imported key is left");
    assert_eq!(seed.into_iter().sum::<Amount>(), bobs_balance);

    // Postcondition: the key that the app of Bob found is the private key of his trade output.
    // The wallet of Bob holds one imported key. A message that is signed with it is verified
    // with the public key of the trade output, which is read from the chain.
    let bobs_keys = setup.bob.imported_keys()?;
    let [bobs_key] = &bobs_keys[..] else {
        panic!("the wallet of Bob has {} imported keys", bobs_keys.len());
    };
    trade::prove_key_of_output(bobs_key, bobs_output)?;
    // The proof fails for an output that the key does not belong to.
    assert!(trade::prove_key_of_output(bobs_key, alices_output).is_err());
    Ok(())
}
