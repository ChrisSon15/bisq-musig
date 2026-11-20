//! BT1: the use case UC1, a simple transaction. Alice sends Bob 0.1 BTC.

use bdk_wallet::bitcoin::Amount;

use crate::setup::{self, FUNDING, Setup};

/// The amount that Alice sends to Bob: 0.1 BTC.
const AMOUNT: Amount = Amount::from_sat(10_000_000);

#[tokio::test(flavor = "multi_thread")]
async fn bt1_alice_sends_bitcoin_to_bob() -> anyhow::Result<()> {
    // Precondition: both wallets hold 1 BTC.
    let mut setup = Setup::new().await?;

    // UC1: Bob gives Alice an address of his wallet, and Alice sends the amount to it.
    let bobs_address = setup.bob.new_address().await?;
    let txid = setup.alice.send(&bobs_address, AMOUNT).await?;

    // Postcondition: the transaction was mined. It is read from the block as the node of Bob
    // holds it. This node got the transaction and the block only from its peer.
    let tx = &setup.mine(&[txid]).await?[0];
    let height = setup.bob.node.block_count()?;
    let payments: Vec<Amount> = tx
        .output
        .iter()
        .filter(|output| output.script_pubkey == bobs_address.script_pubkey())
        .map(|output| output.value)
        .collect();
    assert_eq!(payments, [AMOUNT], "one output pays Bob the amount");

    // Postcondition: Alice has 0.1 BTC less. She also paid the fee of the transaction, which the
    // test calculates from the chain.
    let fee = setup::fee(&setup.bob.node, tx)?;
    setup::assert_small_fee(fee, "the fee of the payment");
    setup.alice.wait_for_balance(FUNDING - AMOUNT - fee).await?;

    // Postcondition: Bob has 0.1 BTC more.
    setup.bob.wait_for_balance(FUNDING + AMOUNT).await?;

    // Both wallets list the transaction with its block, Alice as a payment and Bob as a receipt.
    for (user, incoming, amount) in [
        (&mut setup.alice, false, AMOUNT + fee),
        (&mut setup.bob, true, AMOUNT),
    ] {
        let listed = user
            .transaction(txid)
            .await?
            .expect("the wallet lists the transaction");
        assert_eq!(u64::from(listed.block_height), height);
        assert_eq!(listed.num_confirmations, 1);
        assert_eq!(listed.incoming, incoming);
        assert_eq!(Amount::from_sat(listed.amount), amount);
    }
    Ok(())
}
