//! BT3: the use case UC3, a happy trade without a close of the wallet. Alice sells Bitcoin worth
//! 0.1 BTC to Bob. Nobody ends the app in the regular way, so the payouts stay in the outputs of
//! the deposit transaction.

use bdk_wallet::bitcoin::Amount;
use rpc::pb::musigrpc::ClosureKind;

use crate::setup::{self, FUNDING, Setup};
use crate::trade::{self, Payouts, Terms};

const TRADE_ID: &str = "bt3";
/// Alice sells 0.1 BTC. The security deposits are those of the plan: 0.01 BTC of Bob, the buyer,
/// and 0.015 BTC of Alice, the seller.
const TERMS: Terms = Terms {
    trade_amount: Amount::from_sat(10_000_000),
    buyers_security_deposit: Amount::from_sat(1_000_000),
    sellers_security_deposit: Amount::from_sat(1_500_000),
};

#[tokio::test(flavor = "multi_thread")]
async fn bt3_alice_sells_bitcoin_to_bob_and_the_payouts_stay() -> anyhow::Result<()> {
    // Precondition: both wallets hold 1 BTC.
    let mut setup = Setup::new().await?;

    // UC3: the trade. Alice is the seller and Bob is the buyer. They set up the trade, Bob
    // publishes the deposit transaction, and after its confirmation they close the trade
    // cooperatively.
    let deposit_tx = trade::set_up_and_confirm_deposit(&mut setup, TRADE_ID, &TERMS).await?;
    let (seller, buyer) = (&mut setup.alice.musig, &mut setup.bob.musig);
    trade::close_cooperatively(seller, buyer, TRADE_ID).await?;
    for user in [&mut setup.alice, &mut setup.bob] {
        let closure = trade::closure(user, TRADE_ID).await?;
        assert_eq!(closure, Some(ClosureKind::Cooperative));
    }

    // Postcondition: Bob has 0.1 BTC more and Alice has 0.1 BTC less, minus the fees. The only
    // transaction of the trade is the deposit transaction. The part of its fee that each trader
    // paid is calculated from the chain.
    let (alice, bob) = (&mut setup.alice, &mut setup.bob);
    let deposit_fees = trade::deposit_fees(alice, bob, &deposit_tx, &TERMS, Payouts::Owned).await?;
    setup::assert_small_fee(deposit_fees.sellers, "the deposit fee of Alice");
    setup::assert_small_fee(deposit_fees.buyers, "the deposit fee of Bob");
    let alices_balance = FUNDING - TERMS.trade_amount - deposit_fees.sellers;
    let bobs_balance = FUNDING + TERMS.trade_amount - deposit_fees.buyers;
    setup.alice.wait_for_balance(alices_balance).await?;
    setup.bob.wait_for_balance(bobs_balance).await?;

    // The other postconditions concern the internal structure of the wallets, which the wallet
    // service does not show. The test stops the `musigd` processes and reads the wallets
    // themselves. The wallets are not closed in the regular way, which would move the payouts.
    setup.alice.stop_app();
    setup.bob.stop_app();

    // Postcondition: Alice and Bob have an imported wallet. It holds the payout of its user as
    // its one coin.
    assert_eq!(setup.alice.coins()?.imported, [TERMS.sellers_payout()]);
    assert_eq!(setup.bob.coins()?.imported, [TERMS.buyers_payout()]);

    // Postcondition: Alice and Bob hold the keys that spend their outputs of the trade. The
    // outputs are read from the chain: they are the outputs of the deposit transaction with the
    // amounts of the payouts.
    let (_, alices_output) = trade::payout_output(&deposit_tx, TERMS.sellers_payout())?;
    let (_, bobs_output) = trade::payout_output(&deposit_tx, TERMS.buyers_payout())?;
    let alices_keys = setup.alice.imported_keys()?;
    let bobs_keys = setup.bob.imported_keys()?;
    let ([alices_key], [bobs_key]) = (&alices_keys[..], &bobs_keys[..]) else {
        panic!(
            "each wallet has one imported key, not {} and {}",
            alices_keys.len(),
            bobs_keys.len()
        );
    };
    trade::prove_key_of_output(alices_key, alices_output)?;
    trade::prove_key_of_output(bobs_key, bobs_output)?;
    // The proof fails for a key that does not belong to the output.
    assert!(trade::prove_key_of_output(alices_key, bobs_output).is_err());
    assert!(trade::prove_key_of_output(bobs_key, alices_output).is_err());
    Ok(())
}
