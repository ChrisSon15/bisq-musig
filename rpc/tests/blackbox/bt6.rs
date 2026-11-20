//! BT6: the use case UC6, a trade that ends with a claim. Alice sells Bitcoin worth 0.2 BTC to
//! Bob. After the deposit transaction Alice stops responding: her app does not run any more. Bob
//! publishes his warning transaction when the warning lock time has passed. Nobody answers it,
//! so the app of Bob claims all funds of the trade when the claim lock time has passed.

use bdk_wallet::bitcoin::{Amount, OutPoint, Sequence};
use protocol::params::MIN_CONFIRMATIONS;
use rpc::pb::musigrpc::{ClosureKind, TradeTxKind};

use crate::setup::{self, CLAIM_LOCK_TIME, FUNDING, Setup, WARNING_LOCK_TIME};
use crate::trade::{self, Payouts, Terms};

const TRADE_ID: &str = "bt6";
/// Alice sells 0.2 BTC. The security deposits are those of the plan: 0.01 BTC of Bob, the buyer,
/// and 0.015 BTC of Alice, the seller.
const TERMS: Terms = Terms {
    trade_amount: Amount::from_sat(20_000_000),
    buyers_security_deposit: Amount::from_sat(1_000_000),
    sellers_security_deposit: Amount::from_sat(1_500_000),
};
#[tokio::test(flavor = "multi_thread")]
async fn bt6_alice_stops_responding_and_bob_claims() -> anyhow::Result<()> {
    // Precondition: both wallets hold 1 BTC.
    let mut setup = Setup::with_short_lock_times().await?;

    // UC6: the trade. Alice is the seller and Bob is the buyer. They set up the trade, and Bob
    // publishes the deposit transaction. The fee rate of the trade is 10 sat per vbyte.
    let deposit_tx = trade::set_up_and_confirm_deposit(&mut setup, TRADE_ID, &TERMS).await?;
    let deposit_height = setup.height_of(deposit_tx.compute_txid())?;
    // No trader holds the key of a payout. Each trader paid his part of the deposit and a part of
    // the fee of the deposit transaction. The parts of the fee are calculated from the chain.
    let (alice, bob) = (&mut setup.alice, &mut setup.bob);
    let deposit_fees =
        trade::deposit_fees(alice, bob, &deposit_tx, &TERMS, Payouts::NotOwned).await?;
    setup::assert_small_fee(deposit_fees.sellers, "the deposit fee of Alice");
    setup::assert_small_fee(deposit_fees.buyers, "the deposit fee of Bob");

    // UC6: Alice stops responding. Her app does not run from here on, so nobody answers the
    // warning transaction of Bob.
    setup.alice.stop_app();

    // The app of Bob does not publish the warning transaction before the warning lock time has
    // passed.
    let too_early = trade::publish_warning(&mut setup.bob.musig, TRADE_ID).await;
    assert!(too_early.is_err(), "the warning lock time has not passed");

    // Postcondition: Bob sent the warning transaction. He asks his app for it when the warning
    // lock time has passed.
    let mut bobs_events = trade::trade_events(&mut setup.bob.musig, TRADE_ID).await?;
    let warning = trade::bob_publishes_warning(&mut setup, TRADE_ID, &deposit_tx, &TERMS).await?;
    let (warning_tx, warning_height, warning_fee) = (warning.tx, warning.height, warning.fee);
    let escrow = &warning_tx.output[0];

    // The claim lock time passes: blocks are mined until the warning transaction has as many
    // confirmations as the claim lock time. Then the app of Bob sends the claim transaction on
    // its own.
    let claim_valid_after = warning_height + u64::from(CLAIM_LOCK_TIME) - 1;
    trade::mine_to_height(&mut setup, TRADE_ID, claim_valid_after).await?;
    let claim_txid = trade::wait_for_sent(&mut bobs_events, TradeTxKind::ClaimTx).await?;
    let claim_tx = setup.mine(&[claim_txid]).await?.remove(0);
    let claim_height = setup.height()?;
    for _ in 1..MIN_CONFIRMATIONS {
        setup.mine_block().await?;
    }
    trade::wait_for_closure(&mut setup.bob, TRADE_ID, ClosureKind::BuyersClaim).await?;

    // Postcondition: Bob sent the claim transaction. The transaction of the chain spends the
    // escrow output of the warning transaction with the claim lock time, through the script of
    // the claim, and pays everything, less its fee, to the wallet of Bob.
    let [claim_input] = &claim_tx.input[..] else {
        panic!("the claim transaction has {} inputs", claim_tx.input.len());
    };
    let escrow_output = OutPoint::new(warning_tx.compute_txid(), 0);
    assert_eq!(claim_input.previous_output, escrow_output);
    assert_eq!(claim_input.sequence, Sequence::from_height(CLAIM_LOCK_TIME));
    assert_eq!(
        claim_input.witness.len(),
        3,
        "a spend with the script path: signature, script and control block"
    );
    let claim_fee = setup::fee(&setup.bob.node, &claim_tx)?;
    setup::assert_small_fee(claim_fee, "the fee of the claim transaction");
    assert_eq!(
        trade::gain_by(&mut setup.bob, &claim_tx).await?,
        escrow.value - claim_fee
    );

    // Postcondition: the warning lock time and the claim lock time have passed since the deposit
    // transaction. The heights are the heights of the blocks that contain the transactions.
    assert!(warning_height - deposit_height >= u64::from(WARNING_LOCK_TIME));
    assert!(claim_height - warning_height >= u64::from(CLAIM_LOCK_TIME));
    let lock_times = u64::from(WARNING_LOCK_TIME) + u64::from(CLAIM_LOCK_TIME);
    assert!(claim_height - deposit_height >= lock_times);

    // Postcondition: Bob holds all funds of the trade. He has his 1 BTC, the trade amount of
    // 0.2 BTC and the security deposit of Alice of 0.015 BTC, minus the fees.
    let all_funds = FUNDING + TERMS.trade_amount + TERMS.sellers_security_deposit;
    assert_eq!(all_funds, Amount::from_sat(121_500_000));
    let bobs_balance = all_funds - deposit_fees.buyers - warning_fee - claim_fee;
    setup.bob.wait_for_balance(bobs_balance).await?;
    Ok(())
}
