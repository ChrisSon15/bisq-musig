//! Tests of the long-running compact block filter client against a bitcoind on regtest.
//!
//! The client under test reaches the node only through the peer-to-peer network. The test code
//! uses the RPC interface of the node to mine, to create transactions and to read the mempool.
//! The test IDs refer to section 9.4 of `concept/CompletionPlan.md`.

use std::time::Duration;

use bdk_bitcoind_rpc::bitcoincore_rpc::json::CreateRawTransactionInput;
use bdk_bitcoind_rpc::bitcoincore_rpc::{Client as RpcClient, RpcApi as _};
use bdk_kyoto::TrustedPeer;
use bdk_wallet::bitcoin::{Address, Amount, Network, OutPoint, Transaction, Txid};
use chain::client::{ChainClient, ChainEvent};
use testenv::TestEnv;

const TIMEOUT: Duration = Duration::from_secs(60);

/// Starts the test environment with spendable coins in the wallet of the node.
///
/// The 101 blocks for this are mined before a client connects. In a test, a connected client
/// needed more than a minute to follow such a burst of new blocks, while it follows single new
/// blocks at once.
fn test_env() -> anyhow::Result<TestEnv> {
    let mut env = TestEnv::new()?;
    env.mine_blocks(101)?;
    Ok(env)
}

fn start_client(env: &TestEnv) -> ChainClient {
    let peer = TrustedPeer::from_socket_addr(env.p2p_socket_addr().expect("peer address"));
    ChainClient::start(Network::Regtest, vec![peer], None)
}

async fn synced_to_tip(client: &ChainClient, env: &TestEnv) -> u32 {
    let height = u32::try_from(env.block_count().expect("block count")).expect("small height");
    tokio::time::timeout(TIMEOUT, client.wait_until_synced_to(height))
        .await
        .expect("the client syncs in time")
        .expect("the client runs");
    height
}

/// The output of the transaction `txid` that pays to `address`.
fn output_to(rpc: &RpcClient, txid: Txid, address: &Address) -> OutPoint {
    let tx = rpc.get_raw_transaction(&txid, None).expect("known tx");
    let vout = tx
        .output
        .iter()
        .position(|output| output.script_pubkey == address.script_pubkey())
        .expect("the transaction pays to the address");
    OutPoint::new(txid, u32::try_from(vout).expect("small index"))
}

/// A transaction that spends `coin` of the node's wallet to a new address of the node, signed
/// but not sent.
fn signed_spend(env: &TestEnv, rpc: &RpcClient, coin: OutPoint, amount: Amount) -> Transaction {
    let input = CreateRawTransactionInput {
        txid: coin.txid,
        vout: coin.vout,
        sequence: None,
    };
    let receiver = env.new_address().expect("new address");
    let outputs = std::iter::once((receiver.to_string(), amount)).collect();
    let unsigned = rpc
        .create_raw_transaction(&[input], &outputs, None, None)
        .expect("raw transaction");
    let signed = rpc
        .sign_raw_transaction_with_wallet(&unsigned, None, None)
        .expect("signed by the wallet of the node");
    assert!(signed.complete);
    signed.transaction().expect("valid transaction")
}

/// The client finds a payment to a watched script and the spend of the watched output, counts
/// confirmations, and a client that is started later finds the same in the blocks that were
/// mined while it did not run.
#[tokio::test(flavor = "multi_thread")]
async fn finds_payments_spends_and_confirmations() -> anyhow::Result<()> {
    let mut env = test_env()?;
    let rpc = env.bitcoin_core_rpc_client()?;
    let client = start_client(&env);
    let mut events = client.subscribe();

    let address = env.new_address()?;
    client.watch_script(address.script_pubkey());
    let funding_txid = env.fund_address(&address, Amount::from_sat(1_000_000))?;
    // The client sees confirmed transactions only.
    synced_to_tip(&client, &env).await;
    assert_eq!(client.confirmations(&funding_txid), 0);
    assert!(client.transaction(&funding_txid).is_none());

    env.mine_block()?;
    let height = synced_to_tip(&client, &env).await;
    let found = client
        .transaction(&funding_txid)
        .expect("found in the block");
    assert_eq!(found.height, height);
    assert_eq!(client.confirmations(&funding_txid), 1);
    let mut confirmed = None;
    while confirmed.is_none() {
        if let ChainEvent::Confirmed { txid, height } =
            tokio::time::timeout(TIMEOUT, events.recv()).await??
        {
            confirmed = Some((txid, height));
        }
    }
    assert_eq!(confirmed, Some((funding_txid, height)));

    env.mine_blocks(2)?;
    synced_to_tip(&client, &env).await;
    assert_eq!(client.confirmations(&funding_txid), 3);

    // The spend of the watched output pays to a script that is not watched.
    let coin = output_to(&rpc, funding_txid, &address);
    assert!(client.spending_tx(&coin).is_none());
    let spend = signed_spend(&env, &rpc, coin, Amount::from_sat(990_000));
    let spend_txid = rpc.send_raw_transaction(&spend)?;
    env.mine_block()?;
    let spend_height = synced_to_tip(&client, &env).await;
    let spending_tx = client.spending_tx(&coin).expect("the spend is found");
    assert_eq!(spending_tx.tx.compute_txid(), spend_txid);
    assert_eq!(spending_tx.height, spend_height);
    client.shutdown()?;

    // A client that starts later evaluates the blocks that were mined while it did not run.
    env.mine_blocks(2)?;
    let restarted = start_client(&env);
    restarted.watch_script(address.script_pubkey());
    synced_to_tip(&restarted, &env).await;
    assert_eq!(restarted.confirmations(&funding_txid), 6);
    assert_eq!(
        restarted.spending_tx(&coin).map(|found| found.height),
        Some(spend_height)
    );

    // A script that is added later is found by a new evaluation of the earlier blocks.
    let other_address = env.new_address()?;
    let other_txid = env.fund_address(&other_address, Amount::from_sat(500_000))?;
    env.mine_block()?;
    let other_height = synced_to_tip(&restarted, &env).await;
    assert!(restarted.transaction(&other_txid).is_none());
    restarted.watch_script(other_address.script_pubkey());
    restarted.rescan_from(other_height - 1)?;
    tokio::time::timeout(TIMEOUT, async {
        while restarted.transaction(&other_txid).is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await?;
    assert_eq!(restarted.confirmations(&other_txid), 1);
    restarted.shutdown()?;
    Ok(())
}

/// The client reports a reorganisation, forgets the transactions of the replaced blocks, and
/// finds them again when a new block contains them.
#[tokio::test(flavor = "multi_thread")]
async fn reorganisation_is_reported() -> anyhow::Result<()> {
    let mut env = test_env()?;
    let rpc = env.bitcoin_core_rpc_client()?;
    let client = start_client(&env);
    let mut events = client.subscribe();

    let address = env.new_address()?;
    client.watch_script(address.script_pubkey());
    let txid = env.fund_address(&address, Amount::from_sat(1_000_000))?;
    let block_hash = env.mine_block()?;
    let height = synced_to_tip(&client, &env).await;
    assert_eq!(client.confirmations(&txid), 1);

    // The block with the transaction is replaced by two blocks. The node puts the transaction
    // back into its mempool, and one of the new blocks contains it.
    rpc.invalidate_block(&block_hash)?;
    env.mine_blocks(2)?;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Ok(ChainEvent::Reorganized { from_height }) = events.recv().await {
                return from_height;
            }
        }
    })
    .await
    .map(|from_height| assert_eq!(from_height, height))?;
    let new_height = synced_to_tip(&client, &env).await;
    assert_eq!(new_height, height + 1);
    let found = client.transaction(&txid).expect("found in a new block");
    assert_ne!(found.block_hash, block_hash);
    assert_eq!(client.confirmations(&txid), new_height - found.height + 1);
    client.shutdown()?;
    Ok(())
}

/// J11 for the chain interface: the client sends a transaction to the node over the peer-to-peer
/// network, without any use of the node's RPC interface, and later finds it in a block.
#[tokio::test(flavor = "multi_thread")]
async fn broadcast_reaches_the_node() -> anyhow::Result<()> {
    let mut env = test_env()?;
    let rpc = env.bitcoin_core_rpc_client()?;
    let client = start_client(&env);

    let address = env.new_address()?;
    let funding_txid = env.fund_address(&address, Amount::from_sat(1_000_000))?;
    env.mine_block()?;
    synced_to_tip(&client, &env).await;
    let coin = output_to(&rpc, funding_txid, &address);
    client.watch_output(coin, address.script_pubkey());

    let spend = signed_spend(&env, &rpc, coin, Amount::from_sat(990_000));
    let txid = client.broadcast(spend).await?;
    tokio::time::timeout(TIMEOUT, async {
        while rpc.get_mempool_entry(&txid).is_err() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the node accepted the transaction into its mempool");
    // The client does not know the transaction before a block contains it.
    assert_eq!(client.confirmations(&txid), 0);

    env.mine_block()?;
    synced_to_tip(&client, &env).await;
    assert_eq!(client.confirmations(&txid), 1);
    assert_eq!(
        client
            .spending_tx(&coin)
            .map(|found| found.tx.compute_txid()),
        Some(txid)
    );
    client.shutdown()?;
    Ok(())
}
