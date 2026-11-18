//! Tests of the long-running compact block filter client against a bitcoind on regtest.
//!
//! The client under test reaches the node only through the peer-to-peer network. The test code
//! uses the RPC interface of the node to mine, to create transactions and to read the mempool.
//! The test IDs refer to section 9.4 of `concept/CompletionPlan.md`.

use std::time::Duration;

use bdk_bitcoind_rpc::bitcoincore_rpc::json::{CreateRawTransactionInput, SignRawTransactionInput};
use bdk_bitcoind_rpc::bitcoincore_rpc::{Client as RpcClient, RpcApi as _};
use bdk_kyoto::TrustedPeer;
use bdk_wallet::bitcoin::{
    Address, Amount, Network, OutPoint, Transaction, TxIn, TxOut, Txid, absolute, transaction,
};
use bdk_wallet::serde_json;
use chain::client::{ChainClient, ChainEvent, ClientError, HashCheckpoint, PeerProblem};
use testenv::{Config, TestEnv};

const TIMEOUT: Duration = Duration::from_secs(60);

/// Starts the test environment with spendable coins in the wallet of the node.
///
/// The 101 blocks for this are mined before a client connects. In a test, a connected client
/// needed more than a minute to follow such a burst of new blocks, while it follows single new
/// blocks at once.
fn test_env() -> anyhow::Result<TestEnv> {
    test_env_with(Config::default())
}

fn test_env_with(config: Config<'_>) -> anyhow::Result<TestEnv> {
    let mut env = TestEnv::new_with_conf(config)?;
    env.mine_blocks(101)?;
    Ok(env)
}

/// Waits until `condition` holds.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(TIMEOUT, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timeout: {what}"));
}

/// The block of the node at `height`, as a start point for a client.
fn checkpoint(rpc: &RpcClient, height: u32) -> HashCheckpoint {
    let hash = rpc.get_block_hash(u64::from(height)).expect("known block");
    HashCheckpoint::new(height, hash)
}

fn start_client(env: &TestEnv) -> ChainClient {
    let peer = TrustedPeer::from_socket_addr(env.p2p_socket_addr().expect("peer address"));
    ChainClient::started(Network::Regtest, vec![peer], None)
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

/// J5 for the chain interface: while the only peer is not reachable, the client reports the
/// problem, a broadcast fails, and the client keeps trying. When the peer is back, the client
/// evaluates the blocks that were mined in between, and a broadcast reaches the node.
#[tokio::test(flavor = "multi_thread")]
async fn j5_client_connects_again_after_the_peer_was_unreachable() -> anyhow::Result<()> {
    let mut env = test_env()?;
    let rpc = env.bitcoin_core_rpc_client()?;
    let client = start_client(&env);
    let address = env.new_address()?;
    client.watch_script(address.script_pubkey());
    let funding_txid = env.fund_address(&address, Amount::from_sat(1_000_000))?;
    env.mine_block()?;
    synced_to_tip(&client, &env).await;
    assert_eq!(client.confirmations(&funding_txid), 1);
    assert_eq!(client.peer_problem(), None);
    let coin = output_to(&rpc, funding_txid, &address);
    let spend = signed_spend(&env, &rpc, coin, Amount::from_sat(990_000));

    // The node closes all connections and accepts no new one.
    rpc.set_network_active(false)?;
    eventually("the client reports that no peer is reachable", || {
        client.peer_problem() == Some(PeerProblem::NoPeerReachable)
    })
    .await;
    assert!(
        client.view().is_none(),
        "a client without a peer is not synced"
    );
    assert_eq!(
        client.synced_view().await.err(),
        Some(ClientError::Peers(PeerProblem::NoPeerReachable))
    );
    let error = client
        .broadcast(spend.clone())
        .await
        .expect_err("nothing can be sent without a peer");
    assert!(
        matches!(
            error,
            ClientError::NotConnected | ClientError::NotRequested(_) | ClientError::Node(_)
        ),
        "{error}"
    );
    // What the client knows stays as it was.
    assert_eq!(client.confirmations(&funding_txid), 1);
    env.mine_blocks(2)?;

    rpc.set_network_active(true)?;
    synced_to_tip(&client, &env).await;
    assert_eq!(client.confirmations(&funding_txid), 3);
    eventually("the problem with the peers is over", || {
        client.peer_problem().is_none()
    })
    .await;
    let txid = client.broadcast(spend).await?;
    eventually("the node has the transaction", || {
        rpc.get_mempool_entry(&txid).is_ok()
    })
    .await;
    env.mine_block()?;
    synced_to_tip(&client, &env).await;
    assert_eq!(
        client
            .spending_tx(&coin)
            .map(|found| found.tx.compute_txid()),
        Some(txid)
    );
    client.shutdown()?;
    Ok(())
}

/// A reorganisation that happens while the client has no connection is noticed when the client
/// connects again: it is reported, and the transaction is found in its new block.
#[tokio::test(flavor = "multi_thread")]
async fn reorganisation_without_a_connection_is_noticed_later() -> anyhow::Result<()> {
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

    rpc.set_network_active(false)?;
    eventually("the client has lost its peer", || client.view().is_none()).await;
    rpc.invalidate_block(&block_hash)?;
    env.mine_blocks(3)?;
    rpc.set_network_active(true)?;

    let from_height = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Ok(ChainEvent::Reorganized { from_height }) = events.recv().await {
                return from_height;
            }
        }
    })
    .await?;
    assert_eq!(from_height, height);
    let new_height = synced_to_tip(&client, &env).await;
    assert_eq!(new_height, height + 2);
    let found = client.transaction(&txid).expect("found in a new block");
    assert_ne!(found.block_hash, block_hash);
    assert_eq!(client.confirmations(&txid), new_height - found.height + 1);
    client.shutdown()?;
    Ok(())
}

/// J9 for the chain interface: a transaction that the node does not accept is sent without an
/// error, because a broadcast has no result. The client never reports it as confirmed. A second
/// send before the next block ends without a request of the peer, and does not block.
#[tokio::test(flavor = "multi_thread")]
async fn j9_rejected_transaction_is_never_reported_as_confirmed() -> anyhow::Result<()> {
    let mut env = test_env()?;
    let rpc = env.bitcoin_core_rpc_client()?;
    let client = start_client(&env);
    let address = env.new_address()?;
    let funding_txid = env.fund_address(&address, Amount::from_sat(1_000_000))?;
    env.mine_block()?;
    synced_to_tip(&client, &env).await;
    let coin = output_to(&rpc, funding_txid, &address);
    client.watch_output(coin, address.script_pubkey());

    // The node spends the coin in another transaction, which makes the first one invalid.
    let rejected = signed_spend(&env, &rpc, coin, Amount::from_sat(990_000));
    let accepted = signed_spend(&env, &rpc, coin, Amount::from_sat(980_000));
    let accepted_txid = rpc.send_raw_transaction(&accepted)?;
    env.mine_block()?;
    synced_to_tip(&client, &env).await;
    assert!(env.broadcast(&rejected).is_err(), "the node rejects it");

    let rejected_txid = client.broadcast(rejected.clone()).await?;
    assert_eq!(client.confirmations(&rejected_txid), 0);
    // The peer does not ask again for a transaction that it has just rejected.
    let error = client
        .broadcast(rejected.clone())
        .await
        .expect_err("the peer does not ask for the transaction");
    assert!(matches!(error, ClientError::NotRequested(_)), "{error}");

    env.mine_blocks(2)?;
    synced_to_tip(&client, &env).await;
    assert_eq!(client.confirmations(&rejected_txid), 0);
    assert!(client.transaction(&rejected_txid).is_none());
    assert!(rpc.get_mempool_entry(&rejected_txid).is_err());
    // The client reports the transaction that spent the output instead.
    assert_eq!(
        client
            .spending_tx(&coin)
            .map(|found| found.tx.compute_txid()),
        Some(accepted_txid)
    );
    client.shutdown()?;
    Ok(())
}

/// J10 for the chain interface: a peer that does not serve compact block filters gives a clear
/// problem, and the client is never synced.
#[tokio::test(flavor = "multi_thread")]
async fn j10_peer_without_block_filters_is_reported() -> anyhow::Result<()> {
    let mut config = Config::default();
    config
        .bitcoind
        .args
        .retain(|arg| !arg.starts_with("-peerblockfilters"));
    let env = test_env_with(config)?;
    let client = start_client(&env);
    eventually("the client reports the missing filters", || {
        client.peer_problem() == Some(PeerProblem::NoCompactFilters)
    })
    .await;
    assert!(client.view().is_none());
    assert_eq!(
        client.synced_view().await.err(),
        Some(ClientError::Peers(PeerProblem::NoCompactFilters))
    );
    assert_eq!(client.synced_height(), None);
    // The problem stays while the client keeps trying.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(client.peer_problem(), Some(PeerProblem::NoCompactFilters));
    client.shutdown()?;
    Ok(())
}

/// Fills the mempool of the node with large transactions that pay 20 sat/vB, until the node
/// has removed some of them again and has raised its minimum fee for new transactions.
fn fill_mempool(env: &TestEnv, rpc: &RpcClient) -> anyhow::Result<()> {
    const OUTPUT_COUNT: u64 = 3_000;
    const OUTPUT_VALUE: Amount = Amount::from_sat(1_000);
    const FEE: Amount = Amount::from_sat(1_900_000);
    let receiver = env.new_address()?.script_pubkey();
    // Only the large coins: the rewards of mined blocks.
    let coins = rpc
        .list_unspent(Some(1), None, None, None, None)?
        .into_iter()
        .filter(|coin| coin.amount >= Amount::ONE_BTC);
    for coin in coins {
        // About 93 000 vbytes: one input, 3 000 small outputs and the rest of the coin.
        let rest = coin.amount - OUTPUT_VALUE * OUTPUT_COUNT - FEE;
        let mut output = vec![
            TxOut {
                value: OUTPUT_VALUE,
                script_pubkey: receiver.clone(),
            };
            usize::try_from(OUTPUT_COUNT)?
        ];
        output.push(TxOut {
            value: rest,
            script_pubkey: receiver.clone(),
        });
        let unsigned = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(coin.txid, coin.vout),
                ..TxIn::default()
            }],
            output,
        };
        let signed = rpc.sign_raw_transaction_with_wallet(&unsigned, None, None)?;
        assert!(signed.complete);
        // The node rejects a transaction that does not fit into the full mempool, and it removes
        // other transactions to get below its limit. Both raise the minimum fee.
        let full = match rpc.send_raw_transaction(&signed.transaction()?) {
            Ok(_) => false,
            Err(error) if error.to_string().contains("mempool full") => true,
            Err(error) => return Err(error.into()),
        };
        // 0.00001 BTC per 1 000 vbytes (1 sat/vB) is the minimum fee of a mempool with space.
        let minimum_fee = rpc.call::<serde_json::Value>("getmempoolinfo", &[])?["mempoolminfee"]
            .as_f64()
            .expect("mempoolminfee");
        if minimum_fee > 0.000_05 {
            return Ok(());
        }
        assert!(!full, "a full mempool has a raised minimum fee");
    }
    anyhow::bail!("the node has too few coins to fill its mempool")
}

/// J7 (point N3 of the plan): the mempool of the node is full, so its minimum fee lies above the
/// fee rate of a transaction that was signed earlier. The node rejects this transaction alone.
/// It accepts it when the client sends it over the peer-to-peer network in a package with a
/// child that pays for both.
#[tokio::test(flavor = "multi_thread")]
async fn j7_package_of_low_fee_parent_and_child_is_accepted() -> anyhow::Result<()> {
    let mut config = Config::default();
    // The smallest mempool that the node allows, in megabytes.
    config.bitcoind.args.push("-maxmempool=5");
    let mut env = TestEnv::new_with_conf(config)?;
    // Many spendable coins for the transactions that fill the mempool.
    env.mine_blocks(250)?;
    let rpc = env.bitcoin_core_rpc_client()?;
    let client = start_client(&env);
    let address = env.new_address()?;
    let funding_txid = env.fund_address(&address, Amount::from_sat(1_000_000))?;
    env.mine_block()?;
    synced_to_tip(&client, &env).await;
    let coin = output_to(&rpc, funding_txid, &address);

    // The parent pays 300 sat, which is between 2 and 3 sat/vB. This is above the fixed minimum
    // relay fee of the node of 1 sat/vB.
    let parent = signed_spend(&env, &rpc, coin, Amount::from_sat(999_700));
    let parent_txid = parent.compute_txid();
    // The child spends the output of the parent and pays 20 000 sat.
    let input = CreateRawTransactionInput {
        txid: parent_txid,
        vout: 0,
        sequence: None,
    };
    let outputs =
        std::iter::once((env.new_address()?.to_string(), Amount::from_sat(979_700))).collect();
    let unsigned = rpc.create_raw_transaction(&[input], &outputs, None, None)?;
    let parent_output = SignRawTransactionInput {
        txid: parent_txid,
        vout: 0,
        script_pub_key: parent.output[0].script_pubkey.clone(),
        redeem_script: None,
        amount: Some(parent.output[0].value),
    };
    let signed = rpc.sign_raw_transaction_with_wallet(&unsigned, Some(&[parent_output]), None)?;
    assert!(signed.complete);
    let child = signed.transaction()?;

    fill_mempool(&env, &rpc)?;
    let error = env
        .broadcast(&parent)
        .expect_err("fee below the minimum of the full mempool");
    assert!(
        error.to_string().contains("mempool min fee not met"),
        "{error}"
    );

    let child_txid = client.broadcast_package(parent, child).await?;
    eventually("the node has accepted parent and child", || {
        rpc.get_mempool_entry(&parent_txid).is_ok() && rpc.get_mempool_entry(&child_txid).is_ok()
    })
    .await;
    client.shutdown()?;
    Ok(())
}

/// A script that is watched from an earlier height is found in the blocks that the client had
/// passed already, also below the block at which the client started.
#[tokio::test(flavor = "multi_thread")]
async fn watching_from_an_earlier_height_finds_earlier_payments() -> anyhow::Result<()> {
    let mut env = test_env()?;
    let rpc = env.bitcoin_core_rpc_client()?;
    let early_address = env.new_address()?;
    let early_txid = env.fund_address(&early_address, Amount::from_sat(300_000))?;
    env.mine_block()?;
    let early_height = u32::try_from(env.block_count()?)?;
    env.mine_blocks(3)?;
    let start_height = u32::try_from(env.block_count()?)?;

    let peer = TrustedPeer::from_socket_addr(env.p2p_socket_addr().expect("peer address"));
    let start = checkpoint(&rpc, start_height);
    let client = ChainClient::started(Network::Regtest, vec![peer], Some(start));
    let late_address = env.new_address()?;
    let late_txid = env.fund_address(&late_address, Amount::from_sat(400_000))?;
    env.mine_block()?;
    let late_height = synced_to_tip(&client, &env).await;
    env.mine_block()?;
    synced_to_tip(&client, &env).await;
    assert!(client.view().is_some());

    // Above the start block: the connection evaluates the blocks again.
    assert!(client.watch_from([late_address.script_pubkey()], [], late_height - 1));
    assert!(
        client.view().is_none(),
        "not synced during the new evaluation"
    );
    let view = tokio::time::timeout(TIMEOUT, client.synced_view()).await??;
    assert_eq!(
        view.txs
            .iter()
            .map(|found| found.tx.compute_txid())
            .collect::<Vec<_>>(),
        vec![late_txid]
    );
    // The same request again needs nothing.
    assert!(!client.watch_from([late_address.script_pubkey()], [], late_height - 1));
    assert!(client.view().is_some());

    // Below the start block: the client needs a lower base first.
    let base = checkpoint(&rpc, early_height - 1);
    client.lower_base(base);
    assert_eq!(
        client.base().map(|base| base.height),
        Some(early_height - 1)
    );
    assert!(client.watch_from([early_address.script_pubkey()], [], early_height - 1));
    let view = tokio::time::timeout(TIMEOUT, client.synced_view()).await??;
    assert_eq!(view.base_height(), early_height - 1);
    assert_eq!(view.block_hash(early_height - 1), Some(base.hash));
    assert_eq!(
        view.blocks_above(early_height - 1).count(),
        (view.tip.0 - (early_height - 1)) as usize
    );
    let found = client
        .transaction(&early_txid)
        .expect("found below the start");
    assert_eq!(found.height, early_height);
    assert_eq!(client.confirmations(&late_txid), 2);
    client.shutdown()?;
    Ok(())
}
