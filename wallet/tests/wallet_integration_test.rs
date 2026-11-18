use std::fs;
use std::str::FromStr as _;

use base64::Engine as _;
use bdk_bitcoind_rpc::bitcoincore_rpc::RpcApi as _;
use bdk_kyoto::bip157::tokio;
use bdk_kyoto::{FeeRate, TrustedPeer};
use bdk_wallet::bitcoin::{Address, Amount, Network, Transaction};
use bdk_wallet::error::CreateTxError::OutputBelowDustLimit;
use bdk_wallet::psbt::PsbtUtils as _;
use bdk_wallet::{KeychainKind, SignOptions, serde_json};
use chain::client::{ChainClient, ClientError, PeerProblem};
use rand::RngCore as _;
use rusqlite::Connection;
use secp::Scalar;
use tempfile::{TempDir, tempdir};
use testenv::TestEnv;
use wallet::bmp_wallet::*;
use wallet::error::{ChainDataSourceError, WalletErrorKind};
use wallet::password::Password;
use wallet::persisted::DBStorage;
use wallet::protocol_wallet_api::ProtocolWalletApi as _;
use wallet::utils::derive_key_from_password;
use zeroize::Zeroizing;

/// A password that follows the wallet's password rules, for tests that need some password.
const TEST_PASSWORD: &str = "Test-Passw0rd";

/// [`TEST_PASSWORD`] as a [`Password`], for creating wallets and setting passwords.
fn test_password() -> Password {
    TEST_PASSWORD
        .parse()
        .expect("TEST_PASSWORD follows the password rules")
}

/// The wallet sync over compact block filters, with the node of the test as the only peer. This
/// is the long-running client that the wallet uses in operation. The first sync starts it.
fn scanner(env: &TestEnv) -> ChainClient {
    ChainClient::new(
        Network::Regtest,
        vec![TrustedPeer::from_socket_addr(
            env.p2p_socket_addr().expect("peer address of the node"),
        )],
    )
}

/// Syncs the wallet when the client has evaluated all blocks of the node.
///
/// A client that runs learns of a new block a moment after the node has mined it. A sync at once
/// after the mining could end before that.
async fn sync_to_tip(
    wallet: &mut BMPWallet,
    client: &ChainClient,
    env: &TestEnv,
) -> anyhow::Result<()> {
    if client.is_started() {
        let height = u32::try_from(env.block_count()?)?;
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            client.wait_until_synced_to(height),
        )
        .await??;
    }
    wallet.sync_all(client).await?;
    Ok(())
}

fn new_private_key() -> Scalar {
    let mut seed: [u8; 32] = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    Scalar::from_slice(&seed).unwrap()
}

#[tokio::test]
async fn init_test() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    let chain = scanner(&env);

    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    let receive_amount = Amount::from_sat(100_000);

    let receiving_addr = wallet.next_unused_address(KeychainKind::External);

    env.fund_address(&receiving_addr, receive_amount)?;
    env.mine_block()?;

    sync_to_tip(&mut wallet, &chain, &env).await?;

    assert_eq!(wallet.balance(), receive_amount);
    Ok(())
}

#[tokio::test]
async fn test_sync_with_imported_keys() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    let chain = scanner(&env);

    let prv_key = new_private_key();

    let receive_amount = Amount::from_sat(100_000);

    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    wallet.import_private_key(prv_key, None)?;

    let receiving_addr = wallet.next_unused_address(KeychainKind::External);

    env.fund_address(&receiving_addr, receive_amount)?;
    env.fund_from_prv_key(&prv_key, receive_amount)?;
    env.mine_block()?;

    sync_to_tip(&mut wallet, &chain, &env).await?;
    assert_eq!(wallet.balance(), receive_amount + receive_amount);

    Ok(())
}

#[tokio::test]
async fn test_broadcast_transaction() -> anyhow::Result<()> {
    // This test broadcast a transaction created from main wallet balance only
    let mut env = TestEnv::new()?;
    let chain = scanner(&env);

    let prv_key = new_private_key();

    // Bind the dir once: `env.get_tmp_path()` allocates a fresh `TempDir` on every call,
    // so calling it twice (here and at `load_wallet` below) would point to two different
    // directories. `.to_path_buf()` ends the `&mut env` borrow immediately so the rest of
    // the test can keep mutating `env`.
    let dir = env.new_temp_path().to_path_buf();
    let dir = dir.as_path();
    let mut wallet = BMPWallet::new(dir.into(), &test_password(), Network::Regtest)?;
    wallet.import_private_key(prv_key, None)?;

    let receive_amount = Amount::from_sat(100_000);
    let to_address =
        Address::from_str("tb1pyfv094rr0vk28lf8v9yx3veaacdzg26ztqk4ga84zucqqhafnn5q9my9rz")?;

    let receiving_addr = wallet.next_unused_address(KeychainKind::External);

    env.fund_address(&receiving_addr, receive_amount)?;
    env.mine_block()?;

    sync_to_tip(&mut wallet, &chain, &env).await?;

    let mut tx_builder = wallet.build_tx();
    let send_amount = Amount::from_sat(1_000);
    tx_builder.add_recipient(to_address.assume_checked(), send_amount);

    let mut psbt = tx_builder.finish()?;

    wallet.sign(&mut psbt, SignOptions::default())?;

    let fee = psbt.fee_amount().unwrap();
    // Broadcast the transaction
    env.broadcast(&psbt.extract_tx()?)?;
    env.mine_block()?;

    // Rescan the wallet to apply balance changes
    sync_to_tip(&mut wallet, &chain, &env).await?;

    let new_balance = receive_amount - send_amount - fee;
    assert_eq!(wallet.balance(), new_balance);

    // Reload the wallet to make sure the state changes are persisted
    let enc_wallet = BMPWallet::load_wallet(dir.into(), Network::Regtest, &test_password())?;
    assert_eq!(enc_wallet.balance(), new_balance);

    Ok(())
}

#[tokio::test]
async fn test_broadcast_transaction_two() -> anyhow::Result<()> {
    // This test broadcast a transaction created from imported wallets only
    let mut env = TestEnv::new()?;
    let chain = scanner(&env);

    let prv_key = new_private_key();

    // See note in `test_broadcast_transaction` re: binding the temp dir once.
    let dir = env.new_temp_path().to_path_buf();
    let dir = dir.as_path();
    let mut wallet = BMPWallet::new(dir.into(), &test_password(), Network::Regtest)?;
    wallet.import_private_key(prv_key, None)?;

    let receive_amount = Amount::from_sat(100_000);
    let to_address =
        Address::from_str("tb1pyfv094rr0vk28lf8v9yx3veaacdzg26ztqk4ga84zucqqhafnn5q9my9rz")?;

    env.fund_from_prv_key(&prv_key, receive_amount)?;
    env.mine_block()?;

    sync_to_tip(&mut wallet, &chain, &env).await?;

    let mut tx_builder = wallet.build_tx();
    let send_amount = Amount::from_sat(1_000);
    tx_builder.add_recipient(to_address.assume_checked(), send_amount);

    let mut psbt = tx_builder.finish()?;

    wallet.sign(&mut psbt, SignOptions::default())?;

    let fee = psbt.fee_amount().unwrap();
    // Broadcast the transaction
    env.broadcast(&psbt.extract_tx()?)?;
    env.mine_block()?;

    // Rescan the wallet to apply balance changes
    sync_to_tip(&mut wallet, &chain, &env).await?;

    let new_balance = receive_amount - send_amount - fee;
    assert_eq!(wallet.balance(), new_balance);

    // Reload the wallet to make sure the state changes are persisted
    let enc_wallet = BMPWallet::load_wallet(dir.into(), Network::Regtest, &test_password())?;
    assert_eq!(enc_wallet.balance(), new_balance);

    Ok(())
}

#[tokio::test]
async fn test_broadcast_transaction_three() -> anyhow::Result<()> {
    // This test will attempt send a transaction created from both main wallet and imported keys
    // balance
    let mut env = TestEnv::new()?;
    let chain = scanner(&env);

    let prv_key = new_private_key();

    // See note in `test_broadcast_transaction` re: binding the temp dir once.
    let dir = env.new_temp_path().to_path_buf();
    let dir = dir.as_path();
    let mut wallet = BMPWallet::new(dir.into(), &test_password(), Network::Regtest)?;
    wallet.import_private_key(prv_key, None)?;

    let main_wallet_addr = wallet.next_unused_address(KeychainKind::External);

    let receive_amount = Amount::from_sat(100_000);
    let to_address =
        Address::from_str("tb1pyfv094rr0vk28lf8v9yx3veaacdzg26ztqk4ga84zucqqhafnn5q9my9rz")?;

    env.fund_from_prv_key(&prv_key, receive_amount)?;
    env.fund_address(&main_wallet_addr, receive_amount)?;

    env.mine_block()?;

    sync_to_tip(&mut wallet, &chain, &env).await?;

    let mut tx_builder = wallet.build_tx();
    let send_amount = Amount::from_sat(100_000);
    tx_builder.add_recipient(to_address.assume_checked(), send_amount);

    let mut psbt = tx_builder.finish()?;

    wallet.sign(&mut psbt, SignOptions::default())?;

    let fee = psbt.fee_amount().unwrap();

    // Broadcast the transaction
    env.broadcast(&psbt.extract_tx()?)?;
    env.mine_block()?;

    // Rescan the wallet to apply balance changes
    sync_to_tip(&mut wallet, &chain, &env).await?;

    let new_balance = (receive_amount + receive_amount) - send_amount - fee;
    assert_eq!(wallet.balance(), new_balance);

    // Reload the wallet to make sure the state changes are persisted
    let mut enc_wallet = BMPWallet::load_wallet(dir.into(), Network::Regtest, &test_password())?;

    env.fund_address(&main_wallet_addr, Amount::from_sat(10_000))?;
    env.mine_block()?;
    sync_to_tip(&mut enc_wallet, &chain, &env).await?;
    assert_eq!(enc_wallet.balance(), new_balance + Amount::from_sat(10_000));

    Ok(())
}

#[tokio::test]
async fn test_cbf_main_wallet() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    env.mine_blocks(2)?;
    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    let addr = wallet.next_unused_address(KeychainKind::External);
    env.fund_address(&addr, Amount::from_sat(100_000))?;

    assert_eq!(wallet.balance(), Amount::from_sat(0));

    env.mine_blocks(4)?;

    let peers = [TrustedPeer::from_socket_addr(
        env.p2p_socket_addr().unwrap(),
    )];
    wallet
        .sync_all(&ChainClient::new(Network::Regtest, peers.to_vec()))
        .await?;
    assert_eq!(wallet.balance(), Amount::from_sat(100_000));
    Ok(())
}

#[tokio::test]
async fn test_cbf_imported() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    env.mine_block()?;

    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;

    let prv_keys = [new_private_key(), new_private_key(), new_private_key()];
    for e in &prv_keys {
        wallet.import_private_key(*e, None)?;
    }
    for e in &prv_keys {
        env.fund_from_prv_key(e, Amount::from_sat(10_000)).unwrap();
    }

    assert_eq!(wallet.balance(), Amount::from_sat(0));

    env.mine_blocks(4)?;
    let peers = vec![TrustedPeer::from_socket_addr(
        env.p2p_socket_addr().unwrap(),
    )];

    wallet
        .sync_all(&ChainClient::new(Network::Regtest, peers))
        .await?;
    assert_eq!(wallet.balance(), Amount::from_sat(30_000));
    Ok(())
}

/// I7 (F19): a key that is imported with a start height. The wallet of the key begins near the
/// tip of the main wallet, the sync with compact block filters works from there, and it finds a
/// coin of the key that is confirmed after the start height.
#[tokio::test]
async fn i7_cbf_sync_of_a_key_imported_with_start_height() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    env.mine_blocks(30)?;
    let peers = vec![TrustedPeer::from_socket_addr(
        env.p2p_socket_addr().unwrap(),
    )];
    let cbf = ChainClient::new(Network::Regtest, peers);

    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    sync_to_tip(&mut wallet, &cbf, &env).await?;
    let tip = wallet.latest_checkpoint().height();
    assert!(tip >= 30, "the main wallet knows the chain up to {tip}");

    let prv_key = new_private_key();
    wallet.import_private_key_since(prv_key, None, tip)?;
    assert_eq!(wallet.imported_wallet_tip_heights(), vec![tip]);

    env.fund_from_prv_key(&prv_key, Amount::from_sat(10_000))?;
    env.mine_blocks(3)?;
    sync_to_tip(&mut wallet, &cbf, &env).await?;
    assert_eq!(
        wallet.imported_balance().confirmed,
        Amount::from_sat(10_000)
    );
    // Both wallets followed the chain to the same tip.
    let new_tip = wallet.latest_checkpoint().height();
    assert!(new_tip >= tip + 3);
    assert_eq!(wallet.imported_wallet_tip_heights(), vec![new_tip]);
    Ok(())
}

#[tokio::test]
async fn test_cbf_imported_and_main() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;

    env.mine_block()?;

    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    let addr = wallet.next_unused_address(KeychainKind::External);
    env.fund_address(&addr, Amount::from_sat(100_000))?;

    let prv_keys = [new_private_key(), new_private_key(), new_private_key()];
    for e in &prv_keys {
        wallet.import_private_key(*e, None)?;
    }
    for e in &prv_keys {
        env.fund_from_prv_key(e, Amount::from_sat(10_000)).unwrap();
    }

    assert_eq!(wallet.balance(), Amount::from_sat(0));

    env.mine_blocks(4)?;
    let peers = vec![TrustedPeer::from_socket_addr(
        env.p2p_socket_addr().unwrap(),
    )];

    wallet
        .sync_all(&ChainClient::new(Network::Regtest, peers))
        .await?;

    assert_eq!(wallet.balance(), Amount::from_sat(130_000));

    Ok(())
}

#[tokio::test]
async fn test_cbf_persistence() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;

    env.mine_block()?;

    // See note in `test_broadcast_transaction` re: binding the temp dir once.
    let dir = env.new_temp_path().to_path_buf();
    let dir = dir.as_path();
    let mut wallet = BMPWallet::new(dir.into(), &test_password(), Network::Regtest)?;
    let addr = wallet.next_unused_address(KeychainKind::External);
    env.fund_address(&addr, Amount::from_sat(230_000))?;

    let peers = [TrustedPeer::from_socket_addr(
        env.p2p_socket_addr().unwrap(),
    )];
    env.mine_block()?;

    let cbf = ChainClient::new(Network::Regtest, peers.to_vec());
    sync_to_tip(&mut wallet, &cbf, &env).await?;
    assert_eq!(wallet.balance(), Amount::from_sat(230_000));

    // Reload the wallet from persisted state
    let mut loaded_wallet = BMPWallet::load_wallet(dir.into(), Network::Regtest, &test_password())?;
    assert_eq!(loaded_wallet.balance(), Amount::from_sat(230_000));

    env.fund_address(&addr, Amount::from_sat(70_000))?;
    env.mine_block()?;
    sync_to_tip(&mut loaded_wallet, &cbf, &env).await?;
    assert_eq!(loaded_wallet.balance(), Amount::from_sat(300_000));

    // Create a transaction and broadcast it to the connected peer
    let receiving_addr =
        Address::from_str("tb1pyfv094rr0vk28lf8v9yx3veaacdzg26ztqk4ga84zucqqhafnn5q9my9rz")?;
    let mut tx_builder = loaded_wallet.build_tx();
    tx_builder.add_recipient(receiving_addr.assume_checked(), Amount::from_sat(70_000));

    let mut psbt = tx_builder.finish()?;

    loaded_wallet.sign(&mut psbt, SignOptions::default())?;

    let fee = psbt.fee_amount().unwrap();

    env.broadcast(&psbt.extract_tx()?)?;
    env.mine_block()?;

    sync_to_tip(&mut loaded_wallet, &cbf, &env).await?;
    assert_eq!(loaded_wallet.balance(), Amount::from_sat(230_000) - fee);

    Ok(())
}

#[tokio::test]
async fn test_drain_wallet_with_main_balance() -> anyhow::Result<()> {
    // This test will attempt to drain the imported wallets UTXOs
    // With the main wallet having some balance it won't be touched.
    let mut env = TestEnv::new()?;
    let chain = scanner(&env);

    env.mine_block()?;

    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    let addr = wallet.next_unused_address(KeychainKind::External);

    let amount_to_send_main_wallet = Amount::from_sat(100_000);
    let amount_to_send_imported = Amount::from_sat(10_000);
    env.fund_address(&addr, amount_to_send_main_wallet)?;

    let prv_keys = [new_private_key(), new_private_key()];
    for e in &prv_keys {
        wallet.import_private_key(*e, None)?;
    }
    for e in &prv_keys {
        env.fund_from_prv_key(e, amount_to_send_imported).unwrap();
    }

    env.mine_block()?;
    sync_to_tip(&mut wallet, &chain, &env).await?;

    // Doing *2 because we have two imported keys with the same amount received 10_000
    let current_balance = amount_to_send_main_wallet + amount_to_send_imported * 2;
    assert_eq!(wallet.balance(), current_balance);

    let mut psbt = wallet.drain_imported_balance(FeeRate::from_sat_per_vb(10).unwrap())?;

    wallet.sign(&mut psbt, SignOptions::default())?;

    let drained_amount = amount_to_send_imported * 2 - psbt.fee()?;

    let tx = psbt.extract_tx()?;

    env.broadcast(&tx)?;
    env.mine_block()?;

    sync_to_tip(&mut wallet, &chain, &env).await?;

    let main_wallet_balance = drained_amount + amount_to_send_main_wallet;

    assert_eq!(wallet.balance(), main_wallet_balance);

    Ok(())
}

/// M1, M3, M7, M9 and I8: the drain of the coins of imported keys at the close of the wallet.
#[tokio::test]
async fn m1_drain_imported_coins() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    let chain = scanner(&env);
    env.mine_block()?;
    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    let fee_rate = FeeRate::from_sat_per_vb(10).unwrap();

    // M7: nothing to drain.
    assert!(wallet.drain_imported_coins(fee_rate)?.is_none());

    let keys = [new_private_key(), new_private_key(), new_private_key()];
    let amount = Amount::from_sat(50_000);
    for key in &keys {
        wallet.import_private_key(*key, None)?;
        env.fund_from_prv_key(key, amount)?;
    }
    env.mine_block()?;
    sync_to_tip(&mut wallet, &chain, &env).await?;
    assert_eq!(wallet.imported_balance().confirmed, amount * 3);

    // M3: a coin that is locked for an open trade is not moved.
    let locked_coin = wallet
        .list_utxos()
        .into_iter()
        .find(|utxo| utxo.imported)
        .map(|utxo| bdk_wallet::bitcoin::OutPoint::new(utxo.tx_id, utxo.vout))
        .expect("an imported coin");
    wallet.lock_utxo(locked_coin)?;

    // M1: one transaction moves the other two coins to one address of the seed wallet. The
    // wallet has stored it and counts the coins as spent before the transaction is sent.
    let drain_tx = wallet
        .drain_imported_coins(fee_rate)?
        .expect("two coins to move");
    assert_eq!(drain_tx.input.len(), 2);
    assert_eq!(drain_tx.output.len(), 1);
    assert!(
        drain_tx
            .input
            .iter()
            .all(|input| input.previous_output != locked_coin)
    );
    assert!(wallet.is_mine(drain_tx.output[0].script_pubkey.clone()));
    assert!(
        wallet
            .list_transactions()
            .iter()
            .any(|tx| tx.tx_id == drain_tx.compute_txid())
    );
    // I8: the transaction pays the given fee rate (10 sat per vbyte; the weight is rounded up).
    let fee = amount * 2 - drain_tx.output[0].value;
    let vsize = drain_tx.weight().to_vbytes_ceil();
    assert!(
        (10 * vsize..=10 * vsize + 30).contains(&fee.to_sat()),
        "fee {fee} for {vsize} vbytes"
    );

    // M9: a second call finds nothing more to move.
    assert!(wallet.drain_imported_coins(fee_rate)?.is_none());

    // M4, M8: until the drain transaction is confirmed, the wallet names it as a transaction
    // that must be sent again.
    let to_send_again = || {
        let outgoing = wallet.unconfirmed_outgoing_transactions();
        outgoing
            .iter()
            .map(|tx| tx.compute_txid())
            .collect::<Vec<_>>()
    };
    assert_eq!(to_send_again(), vec![drain_tx.compute_txid()]);

    env.broadcast(&drain_tx)?;
    env.mine_block()?;
    sync_to_tip(&mut wallet, &chain, &env).await?;
    assert_eq!(wallet.unconfirmed_outgoing_transactions(), vec![]);
    // Only the locked coin is left with the imported keys.
    assert_eq!(wallet.imported_balance().total(), amount);
    assert_eq!(wallet.balance(), amount * 2 - fee);

    // M7: an amount that does not pay for the fee is not moved.
    wallet.unlock_utxo(locked_coin)?;
    let high_fee_rate = FeeRate::from_sat_per_vb(1_000).unwrap();
    assert!(wallet.drain_imported_coins(high_fee_rate)?.is_none());
    assert_eq!(wallet.imported_balance().total(), amount);
    Ok(())
}

#[tokio::test]
async fn test_drain_wallet_no_balance() {
    // In this test drain is called but the wallet doesn't have any imported key
    // insuffucient balance should be thrown
    let mut env = TestEnv::new().unwrap();

    env.mine_block().unwrap();

    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )
    .unwrap();
    let addr = wallet.next_unused_address(KeychainKind::External);

    let amount_to_send_main_wallet = Amount::from_sat(100_000);

    env.fund_address(&addr, amount_to_send_main_wallet).unwrap();
    env.mine_block().unwrap();

    let res = wallet.drain_imported_balance(FeeRate::from_sat_per_vb(10).unwrap());

    assert!(
        matches!(res, Err(WalletErrorKind::CreateTx(OutputBelowDustLimit(_)))),
        "unexpected error: {res:?}"
    );
}

fn get_dir() -> TempDir {
    tempdir().unwrap()
}

/// Simulates a crash *between* the `SQLCipher` re-key and the rename that commits the rotated
/// salt (see `BMPWallet::rekey`): the primary salt no longer matches the database key, only
/// the staged one does. `load_wallet` must complete the interrupted rotation.
#[test]
fn load_recovers_an_interrupted_salt_rotation() -> anyhow::Result<()> {
    let dir = get_dir();
    let db_name = BMPWallet::DB_NAME;
    let storage: DBStorage = dir.path().into();
    let seed = {
        let wallet = BMPWallet::new(storage.clone(), &test_password(), Network::Regtest)?;
        wallet.get_seed_phrase(&test_password())?
    };

    let db_path = dir.path().join(db_name);
    let staged_salt_path = storage.staged_salt_path(db_name).unwrap();

    // Re-key the database to a fresh salt that is staged but not yet committed — exactly
    // the state a crash at rekey's commit point leaves behind.
    let old_salt = storage.load_salt(db_name)?;
    let mut new_salt = [0u8; 16];
    rand::rng().fill_bytes(&mut new_salt);
    fs::write(
        &staged_salt_path,
        base64::engine::general_purpose::STANDARD.encode(new_salt),
    )?;
    {
        let db = Connection::open(&db_path)?;
        let old_key = derive_key_from_password(&test_password(), &old_salt)?;
        db.pragma_update(None, "key", old_key.as_str())?;
        let new_key = derive_key_from_password(&test_password(), &new_salt)?;
        db.pragma_update(None, "rekey", new_key.as_str())?;
    }

    let wallet = BMPWallet::load_wallet(dir.path().into(), Network::Regtest, &test_password())?;
    assert_eq!(
        wallet.get_seed_phrase(&test_password())?,
        seed,
        "recovery must yield the same wallet"
    );
    assert!(
        !std::path::Path::new(&staged_salt_path).exists(),
        "the staged salt must have been committed"
    );

    let salt = storage.load_salt(db_name)?;
    assert_eq!(
        salt,
        new_salt.to_vec(),
        "the committed salt must be the rotated one"
    );
    assert!(
        wallet.check_password(&test_password())?,
        "password still verifies"
    );
    drop(wallet);

    // And a subsequent plain load works off the committed salt.
    let reloaded = BMPWallet::load_wallet(dir.path().into(), Network::Regtest, &test_password())?;
    assert_eq!(reloaded.get_seed_phrase(&test_password())?, seed);

    Ok(())
}

/// A leftover staged salt — from a password change that failed before the re-key — must
/// neither stop nor confuse a normal load, and must be cleaned up.
#[test]
fn load_ignores_and_cleans_a_stale_staged_salt() -> anyhow::Result<()> {
    let dir = get_dir();
    let seed = {
        let wallet = BMPWallet::new(dir.path().into(), &test_password(), Network::Regtest)?;
        wallet.get_seed_phrase(&test_password())?
    };

    let staged_salt_path = dir.path().join(format!("{}.salt.new", BMPWallet::DB_NAME));
    fs::write(&staged_salt_path, "bm90LXRoZS1yZWFsLXNhbHQ=")?; // valid base64, wrong salt

    let wallet = BMPWallet::load_wallet(dir.path().into(), Network::Regtest, &test_password())?;
    assert_eq!(wallet.get_seed_phrase(&test_password())?, seed);
    assert!(
        !staged_salt_path.exists(),
        "the stale staged salt must be cleaned up"
    );

    Ok(())
}

#[test]
fn change_password_rotates_the_key_and_survives_a_reload() -> anyhow::Result<()> {
    let dir = get_dir();
    let new_password: Password = "S3cret!!".parse()?;
    let seed = {
        let mut wallet = BMPWallet::new(dir.path().into(), &test_password(), Network::Regtest)?;
        assert!(
            wallet.check_password(&test_password())?,
            "the password the wallet was created with is the current one"
        );

        let seed = wallet.get_seed_phrase(&test_password())?;
        wallet.change_password(&test_password(), &new_password)?;

        assert!(
            wallet.check_password(&new_password)?,
            "new password must verify"
        );
        assert!(
            !wallet.check_password(&test_password())?,
            "old password must stop verifying"
        );
        assert_eq!(
            wallet.get_seed_phrase(&new_password)?,
            seed,
            "seed must survive the re-key"
        );
        seed
    };

    // The rotated salt and key must both have reached disk.
    {
        let reloaded = BMPWallet::load_wallet(dir.path().into(), Network::Regtest, &new_password)?;
        assert_eq!(reloaded.get_seed_phrase(&new_password)?, seed);
    }

    let stale = BMPWallet::load_wallet(dir.path().into(), Network::Regtest, &test_password());
    assert!(
        stale.is_err() || stale.unwrap().get_seed_phrase(&test_password()).is_err(),
        "the replaced password must no longer open the wallet"
    );

    Ok(())
}

/// Creating over an existing wallet must fail *before* the salt is touched. A caller that
/// treats a failed `load_wallet` (e.g. a wrong password) as "no wallet here" would otherwise
/// rewrite the salt and leave the existing database impossible to decrypt.
#[test]
fn new_refuses_to_overwrite_an_existing_wallet() -> anyhow::Result<()> {
    let dir = get_dir();
    let salt_path = dir.path().join(format!("{}.salt", BMPWallet::DB_NAME));

    let seed = {
        let wallet = BMPWallet::new(dir.path().into(), &test_password(), Network::Regtest)?;
        wallet.get_seed_phrase(&test_password())?
    };
    let salt_before = fs::read(&salt_path)?;

    let Err(err) = BMPWallet::new(
        dir.path().into(),
        &"Different#Pw1".parse()?,
        Network::Regtest,
    ) else {
        panic!("creating over an existing wallet must fail");
    };
    assert!(
        err.to_string().contains("refusing to overwrite"),
        "unexpected error: {err}"
    );

    assert_eq!(
        fs::read(&salt_path)?,
        salt_before,
        "the salt must survive a refused creation, or the wallet becomes unopenable"
    );

    // The original password still opens the original wallet.
    let reloaded = BMPWallet::load_wallet(dir.path().into(), Network::Regtest, &test_password())?;
    assert_eq!(reloaded.get_seed_phrase(&test_password())?, seed);

    Ok(())
}

#[test]
fn load_wallet_rejects_a_wrong_password() {
    let dir = get_dir();
    let dir = dir.path();

    let bmp_wallet = BMPWallet::new(dir.into(), &test_password(), Network::Regtest).unwrap();
    let seed = bmp_wallet.get_seed_phrase(&test_password()).unwrap();

    assert_eq!(seed.word_count(), 24);

    // Try loading the wallet with wrong decryption key should panic
    let wrong: Password = "Wrong#Pw123".parse().unwrap();
    let lw = BMPWallet::load_wallet(dir.into(), Network::Regtest, &wrong);
    let Err(err) = lw else {
        panic!("opening with a wrong password must fail");
    };
    assert!(
        matches!(err, WalletErrorKind::InvalidPassword),
        "unexpected error: {err:?}"
    );
}

/// The wallet of one network is not synced through peers of another network.
#[tokio::test]
async fn sync_refuses_peers_of_another_network() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let mut wallet = BMPWallet::new(dir.path().into(), &test_password(), Network::Regtest)?;
    let client = ChainClient::new(Network::Signet, Vec::new());
    let error = wallet.sync_all(&client).await.expect_err("networks differ");
    assert!(
        matches!(
            error,
            WalletErrorKind::ChainDataSource(ChainDataSourceError::NetworkMismatch {
                wallet: Network::Regtest,
                peers: Network::Signet,
            })
        ),
        "{error}"
    );
    assert!(!client.is_started(), "the client was not started for that");
    Ok(())
}

/// F28: two wallets sync through one client, and a wallet that is loaded again continues with
/// the same client. A reorganisation reaches the wallets: they follow the new blocks, and a
/// payment that is confirmed in another block keeps its amount.
#[tokio::test]
async fn f28_wallets_share_one_client_and_follow_a_reorganisation() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    env.mine_blocks(101)?;
    let client = scanner(&env);
    let dir = env.new_temp_path().to_path_buf();
    let mut first = BMPWallet::new(dir.as_path().into(), &test_password(), Network::Regtest)?;
    let mut second = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    let first_address = first.next_unused_address(KeychainKind::External);
    let second_address = second.next_unused_address(KeychainKind::External);
    env.fund_address(&first_address, Amount::from_sat(100_000))?;
    env.mine_block()?;
    sync_to_tip(&mut first, &client, &env).await?;
    sync_to_tip(&mut second, &client, &env).await?;
    assert_eq!(first.balance(), Amount::from_sat(100_000));
    assert_eq!(second.balance(), Amount::ZERO);

    env.fund_address(&second_address, Amount::from_sat(200_000))?;
    env.mine_block()?;
    let height = u32::try_from(env.block_count()?)?;
    sync_to_tip(&mut first, &client, &env).await?;
    sync_to_tip(&mut second, &client, &env).await?;
    assert_eq!(second.balance(), Amount::from_sat(200_000));
    let replaced_block = env.block_hash(u64::from(height))?;
    assert_eq!(second.latest_checkpoint().hash(), replaced_block);

    // The block with the payment to the second wallet is replaced. The payment is confirmed in
    // a new block.
    env.reorganize(1, 2)?;
    drop(first);
    let mut first =
        BMPWallet::load_wallet(dir.as_path().into(), Network::Regtest, &test_password())?;
    sync_to_tip(&mut first, &client, &env).await?;
    sync_to_tip(&mut second, &client, &env).await?;
    for wallet in [&first, &second] {
        assert_eq!(wallet.latest_checkpoint().height(), height + 1);
        assert_eq!(
            wallet.latest_checkpoint().hash(),
            env.block_hash(u64::from(height) + 1)?
        );
        let block_at_height = wallet
            .checkpoints()
            .find(|checkpoint| checkpoint.height() == height)
            .expect("the wallet knows the block");
        assert_eq!(block_at_height.hash(), env.block_hash(u64::from(height))?);
        assert_ne!(block_at_height.hash(), replaced_block);
    }
    assert_eq!(first.balance(), Amount::from_sat(100_000));
    assert_eq!(second.balance(), Amount::from_sat(200_000));
    client.shutdown()?;
    Ok(())
}

/// A payment to a script far behind the last used one is found in the same sync as the payment
/// that brings this script into the range that the wallet derives.
#[tokio::test]
async fn sync_finds_payments_to_scripts_that_an_earlier_payment_reveals() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    env.mine_blocks(101)?;
    let client = scanner(&env);
    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    sync_to_tip(&mut wallet, &client, &env).await?;

    // A new wallet derives the scripts 0 to 24. Script 20 is among them, script 40 is not.
    let near = wallet.peek_address(KeychainKind::External, 20);
    let far = wallet.peek_address(KeychainKind::External, 40);
    env.fund_address(&near.address, Amount::from_sat(30_000))?;
    env.mine_block()?;
    env.fund_address(&far.address, Amount::from_sat(50_000))?;
    env.mine_block()?;
    sync_to_tip(&mut wallet, &client, &env).await?;
    assert_eq!(wallet.balance(), Amount::from_sat(80_000));
    client.shutdown()?;
    Ok(())
}

/// A key whose coin was confirmed before the block at which the client started (for example the
/// payout of a trade that began in an earlier run of the program) is found: the client follows
/// the chain from the earlier block for it.
#[tokio::test]
async fn sync_of_a_key_imported_with_a_start_below_the_start_of_the_client() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    env.mine_blocks(120)?;
    let prv_key = new_private_key();
    env.fund_from_prv_key(&prv_key, Amount::from_sat(10_000))?;
    env.mine_block()?;
    let coin_height = u32::try_from(env.block_count()?)?;
    env.mine_blocks(30)?;

    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    // The first client gives the wallet the blocks of the chain. The second client is the client
    // of a later run: it starts a few blocks below the tip of the wallet.
    let first_run = scanner(&env);
    sync_to_tip(&mut wallet, &first_run, &env).await?;
    first_run.shutdown()?;
    let client = scanner(&env);
    sync_to_tip(&mut wallet, &client, &env).await?;
    let base_height = client.base().expect("started below the tip").height;
    assert!(base_height > coin_height, "{base_height} > {coin_height}");

    wallet.import_private_key_since(prv_key, None, coin_height - 1)?;
    assert_eq!(wallet.imported_wallet_tip_heights(), vec![coin_height - 1]);
    sync_to_tip(&mut wallet, &client, &env).await?;
    assert_eq!(
        wallet.imported_balance().confirmed,
        Amount::from_sat(10_000)
    );
    assert!(client.base().expect("a base").height < coin_height);
    let tip = wallet.latest_checkpoint().height();
    assert_eq!(wallet.imported_wallet_tip_heights(), vec![tip]);
    client.shutdown()?;
    Ok(())
}

/// J5 and issue #188 for the wallet: while no peer is reachable, a sync ends with an error that
/// names the problem, and the wallet keeps what it knows. When the peer is back, the next sync
/// brings the blocks that were mined in between.
#[tokio::test]
async fn j5_sync_reports_an_unreachable_peer_and_works_again_later() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    env.mine_blocks(101)?;
    let client = scanner(&env);
    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    let address = wallet.next_unused_address(KeychainKind::External);
    env.fund_address(&address, Amount::from_sat(100_000))?;
    env.mine_block()?;
    sync_to_tip(&mut wallet, &client, &env).await?;
    assert_eq!(wallet.balance(), Amount::from_sat(100_000));

    env.set_network_active(false)?;
    env.fund_address(&address, Amount::from_sat(50_000))?;
    env.mine_block()?;
    let error = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            // A sync just after the loss of the peer can still end without an error.
            if let Err(error) = wallet.sync_all(&client).await {
                return error;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await?;
    assert!(
        matches!(
            error,
            WalletErrorKind::ChainDataSource(ChainDataSourceError::Client(ClientError::Peers(
                PeerProblem::NoPeerReachable
            )))
        ),
        "{error}"
    );
    assert_eq!(wallet.balance(), Amount::from_sat(100_000));

    env.set_network_active(true)?;
    sync_to_tip(&mut wallet, &client, &env).await?;
    assert_eq!(wallet.balance(), Amount::from_sat(150_000));
    client.shutdown()?;
    Ok(())
}

/// The trade engine stores the records of its trades in the database of the wallet. A record
/// replaces an earlier record of the same trade, records are never deleted, and they are there
/// again when the wallet is loaded. A wallet that is loaded with a wrong password gives no
/// records, because it does not load at all.
#[test]
fn trade_records_are_stored_in_the_wallet_database() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let mut wallet = BMPWallet::new(dir.path().into(), &test_password(), Network::Regtest)?;
    assert_eq!(wallet.trade_records()?, vec![]);
    wallet.store_trade_record("trade-b", b"first")?;
    wallet.store_trade_record("trade-a", b"other trade")?;
    wallet.store_trade_record("trade-b", b"second")?;
    let expected = vec![
        (
            "trade-a".to_owned(),
            Zeroizing::new(b"other trade".to_vec()),
        ),
        ("trade-b".to_owned(), Zeroizing::new(b"second".to_vec())),
    ];
    assert_eq!(wallet.trade_records()?, expected);
    // A new wallet knows the first block of the chain only.
    assert_eq!(
        wallet.known_block_at_or_below(100).map(|block| block.0),
        Some(0)
    );
    drop(wallet);

    let mut wallet = BMPWallet::load_wallet(dir.path().into(), Network::Regtest, &test_password())?;
    assert_eq!(wallet.trade_records()?, expected);
    drop(wallet);

    // The records are in the encrypted database: the file does not contain their text.
    let database = fs::read(dir.path().join(BMPWallet::DB_NAME))?;
    let contains = |needle: &[u8]| database.windows(needle.len()).any(|part| part == needle);
    assert!(!contains(b"other trade") && !contains(b"trade-a"));
    let wrong_password: Password = "Wr0ng-Password!".parse().expect("valid password");
    assert!(BMPWallet::load_wallet(dir.path().into(), Network::Regtest, &wrong_password).is_err());
    Ok(())
}

/// E9, E10 and F5 for the wallet: the fee of an unconfirmed transaction that pays a small
/// output to the wallet, like the anchor of a warning transaction, is raised with a child
/// transaction. The node holds parent and child with the fee rate that was asked for. A second
/// call replaces the child with one that pays more. When the parent can never confirm, the
/// wallet forgets parent and child, and the coins that the child spent are free again.
#[tokio::test]
async fn e9_fee_bump_child_for_a_transaction_with_a_low_fee() -> anyhow::Result<()> {
    const FUNDING: Amount = Amount::from_sat(1_000_000);
    const ANCHOR: Amount = Amount::from_sat(330);
    let mut env = TestEnv::new()?;
    let rpc = env.bitcoin_core_rpc_client()?;
    let chain = scanner(&env);
    let mut wallets = Vec::new();
    for _ in 0..2 {
        let dir = env.new_temp_path().to_path_buf();
        let mut wallet = BMPWallet::new(dir.as_path().into(), &test_password(), Network::Regtest)?;
        let address = wallet.next_unused_address(KeychainKind::External);
        env.fund_address(&address, FUNDING)?;
        wallets.push(wallet);
    }
    env.mine_block()?;
    let mut payer = wallets.pop().expect("two wallets");
    let mut wallet = wallets.pop().expect("two wallets");
    sync_to_tip(&mut wallet, &chain, &env).await?;
    sync_to_tip(&mut payer, &chain, &env).await?;

    // The parent pays 1 sat per vbyte and has an output of 330 sat to the wallet.
    let low_fee_rate = FeeRate::from_sat_per_vb(1).unwrap();
    let anchor_address = wallet.get_new_address()?.address;
    let parent = payer.send_to_address(&anchor_address, ANCHOR, low_fee_rate)?;
    let parent_fee = FUNDING
        - parent
            .output
            .iter()
            .map(|output| output.value)
            .sum::<Amount>();
    let parent_txid = parent.compute_txid();
    let package_fee_rate = |child: &Transaction, child_fee: Amount| {
        (parent_fee + child_fee) / (parent.weight() + child.weight())
    };
    let fee_in_mempool =
        |txid: &bdk_wallet::bitcoin::Txid| rpc.get_mempool_entry(txid).map(|entry| entry.fees.base);

    // A child that would pay more than the limit is refused, and nothing is recorded.
    let target = FeeRate::from_sat_per_vb(10).unwrap();
    let refused = wallet.create_fee_bump_child(&parent, parent_fee, target, Amount::from_sat(100));
    assert!(
        matches!(&refused, Err(WalletErrorKind::FeeBump(reason)) if reason.contains("limit")),
        "{refused:?}"
    );
    assert_eq!(wallet.unconfirmed_outgoing_transactions(), vec![]);

    let limit = Amount::from_sat(50_000);
    let child = wallet.create_fee_bump_child(&parent, parent_fee, target, limit)?;
    assert!(
        child
            .input
            .iter()
            .any(|input| input.previous_output.txid == parent_txid),
        "the child spends the output of the parent"
    );
    let package = [&parent, &child].map(bdk_wallet::bitcoin::consensus::encode::serialize_hex);
    let _: serde_json::Value = rpc.call("submitpackage", &[serde_json::json!(package)])?;
    let child_fee = fee_in_mempool(&child.compute_txid())?;
    assert_eq!(fee_in_mempool(&parent_txid)?, parent_fee);
    assert!(
        package_fee_rate(&child, child_fee) >= target,
        "parent and child pay {child_fee} + {parent_fee}"
    );

    // The next step of a fee schedule: the new child replaces the first one in the node.
    let higher_target = FeeRate::from_sat_per_vb(20).unwrap();
    let second_child = wallet.create_fee_bump_child(&parent, parent_fee, higher_target, limit)?;
    assert_ne!(second_child.compute_txid(), child.compute_txid());
    env.broadcast(&second_child)?;
    assert!(fee_in_mempool(&child.compute_txid()).is_err(), "replaced");
    let second_fee = fee_in_mempool(&second_child.compute_txid())?;
    assert!(package_fee_rate(&second_child, second_fee) >= higher_target);
    let to_send_again = wallet.unconfirmed_outgoing_transactions();
    assert_eq!(to_send_again.len(), 1, "the first child is forgotten");
    assert_eq!(to_send_again[0].compute_txid(), second_child.compute_txid());

    // A refused call keeps the child that is on the network.
    let refused = wallet.create_fee_bump_child(&parent, parent_fee, target, Amount::from_sat(100));
    assert!(refused.is_err());
    assert_eq!(wallet.unconfirmed_outgoing_transactions(), to_send_again);

    env.mine_block()?;
    sync_to_tip(&mut wallet, &chain, &env).await?;
    assert_eq!(wallet.balance(), FUNDING + ANCHOR - second_fee);
    assert_eq!(wallet.unconfirmed_outgoing_transactions(), vec![]);

    // A parent that never confirms: the wallet forgets it together with its child.
    let anchor_address = wallet.get_new_address()?.address;
    let balance = wallet.balance();
    let dead_parent = payer.send_to_address(&anchor_address, ANCHOR, low_fee_rate)?;
    let dead_parent_fee = Amount::from_sat(200);
    wallet.create_fee_bump_child(&dead_parent, dead_parent_fee, target, limit)?;
    assert_eq!(wallet.unconfirmed_outgoing_transactions().len(), 1);
    wallet.forget_transaction(dead_parent.compute_txid())?;
    assert_eq!(wallet.unconfirmed_outgoing_transactions(), vec![]);
    assert_eq!(wallet.balance(), balance);
    Ok(())
}
