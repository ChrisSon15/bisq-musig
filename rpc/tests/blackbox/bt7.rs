//! BT7: the use case UC7, a trade that ends with a redirect. Alice sells Bitcoin worth 0.2 BTC to
//! Bob. After the deposit transaction Bob publishes his warning transaction. The app of Alice
//! answers it with her redirect transaction, which sends all funds of the trade to the receivers
//! that the traders named at the setup. Neither trader gets the funds of the trade.

use bdk_wallet::bitcoin::{Amount, OutPoint, Sequence};
use protocol::params::MIN_CONFIRMATIONS;
use rpc::pb::musigrpc::{ClosureKind, TradeTxKind};

use crate::setup::{self, CLAIM_LOCK_TIME, FUNDING, Setup};
use crate::trade::{self, Payouts, Terms};

const TRADE_ID: &str = "bt7";
/// Alice sells 0.2 BTC. The security deposits are those of the plan: 0.01 BTC of Bob, the buyer,
/// and 0.015 BTC of Alice, the seller.
const TERMS: Terms = Terms {
    trade_amount: Amount::from_sat(20_000_000),
    buyers_security_deposit: Amount::from_sat(1_000_000),
    sellers_security_deposit: Amount::from_sat(1_500_000),
};

#[tokio::test(flavor = "multi_thread")]
async fn bt7_bob_warns_and_alice_redirects() -> anyhow::Result<()> {
    // Precondition: both wallets hold 1 BTC.
    let mut setup = Setup::with_short_lock_times().await?;

    // UC7: the trade. Alice is the seller and Bob is the buyer. They set up the trade, and Bob
    // publishes the deposit transaction. The fee rate of the trade is 10 sat per vbyte.
    let deposit_tx = trade::set_up_and_confirm_deposit(&mut setup, TRADE_ID, &TERMS).await?;
    // No trader holds the key of a payout. Each trader paid his part of the deposit and a part of
    // the fee of the deposit transaction. The parts of the fee are calculated from the chain.
    let (alice, bob) = (&mut setup.alice, &mut setup.bob);
    let deposit_fees =
        trade::deposit_fees(alice, bob, &deposit_tx, &TERMS, Payouts::NotOwned).await?;
    setup::assert_small_fee(deposit_fees.sellers, "the deposit fee of Alice");
    setup::assert_small_fee(deposit_fees.buyers, "the deposit fee of Bob");

    // UC7 and postcondition: Bob sent the warning transaction. He asks his app for it when the
    // warning lock time has passed.
    let mut alices_events = trade::trade_events(&mut setup.alice.musig, TRADE_ID).await?;
    let warning = trade::bob_publishes_warning(&mut setup, TRADE_ID, &deposit_tx, &TERMS).await?;
    let [escrow, bobs_anchor] = &warning.tx.output[..] else {
        panic!("the warning transaction has two outputs");
    };

    // UC7: Alice issues the redirect transaction. Her app does this on its own when it sees the
    // confirmed warning transaction of Bob; no request asks for it.
    let redirect_txid = trade::wait_for_sent(&mut alices_events, TradeTxKind::RedirectTx).await?;
    let redirect_tx = setup.mine(&[redirect_txid]).await?.remove(0);
    let redirect_height = setup.height()?;
    for _ in 1..MIN_CONFIRMATIONS {
        setup.mine_block().await?;
    }
    // Both apps report the trade as closed by the redirect of the seller.
    for user in [&mut setup.alice, &mut setup.bob] {
        trade::wait_for_closure(user, TRADE_ID, ClosureKind::SellersRedirect).await?;
    }

    // Postcondition: Alice sent the redirect transaction. The transaction of the chain spends
    // the escrow output of the warning transaction with one signature, and with a lock time of
    // one block.
    let [redirect_input] = &redirect_tx.input[..] else {
        panic!(
            "the redirect transaction has {} inputs",
            redirect_tx.input.len()
        );
    };
    let escrow_output = OutPoint::new(warning.tx.compute_txid(), 0);
    assert_eq!(redirect_input.previous_output, escrow_output);
    assert_eq!(redirect_input.sequence, Sequence::from_height(1));
    assert_eq!(redirect_input.witness.len(), 1, "a spend with the key path");
    // It was confirmed in the block after the warning transaction, long before Bob could claim
    // the escrow.
    assert_eq!(redirect_height, warning.height + 1);
    assert!(redirect_height < warning.height + u64::from(CLAIM_LOCK_TIME));
    // Its last output is the anchor, which is a coin of the wallet of Alice. The other outputs
    // pay the receivers of the setup: no trader gets anything else from the transaction.
    let alices_anchor = redirect_tx
        .output
        .last()
        .expect("the redirect transaction has outputs");
    assert_eq!(
        trade::gain_by(&mut setup.alice, &redirect_tx).await?,
        alices_anchor.value
    );
    assert_eq!(
        trade::gain_by(&mut setup.bob, &redirect_tx).await?,
        Amount::ZERO
    );
    let redirect_fee = setup::fee(&setup.bob.node, &redirect_tx)?;
    setup::assert_small_fee(redirect_fee, "the fee of the redirect transaction");
    // The receivers get the funds of the trade, less the two anchors and the fees of the warning
    // and of the redirect transaction.
    let funds_of_the_trade =
        TERMS.trade_amount + TERMS.buyers_security_deposit + TERMS.sellers_security_deposit;
    let (_, receiver_outputs) = redirect_tx
        .output
        .split_last()
        .expect("the redirect transaction has outputs");
    assert!(!receiver_outputs.is_empty(), "an output for a receiver");
    let received: Amount = receiver_outputs.iter().map(|output| output.value).sum();
    assert_eq!(
        received,
        funds_of_the_trade - bobs_anchor.value - alices_anchor.value - warning.fee - redirect_fee
    );
    assert_eq!(received, escrow.value - alices_anchor.value - redirect_fee);

    // Postcondition: Bob has 0.99 BTC and Alice has 0.785 BTC, minus the fees. Each lost what he
    // paid into the deposit. Each got the anchor of his own transaction, which is 330 sat.
    let bob_without_deposit = FUNDING - TERMS.buyers_security_deposit;
    let alice_without_deposit = FUNDING - TERMS.trade_amount - TERMS.sellers_security_deposit;
    assert_eq!(bob_without_deposit, Amount::from_sat(99_000_000));
    assert_eq!(alice_without_deposit, Amount::from_sat(78_500_000));
    let bobs_balance = bob_without_deposit - deposit_fees.buyers + bobs_anchor.value;
    let alices_balance = alice_without_deposit - deposit_fees.sellers + alices_anchor.value;
    setup.bob.wait_for_balance(bobs_balance).await?;
    setup.alice.wait_for_balance(alices_balance).await?;
    Ok(())
}
