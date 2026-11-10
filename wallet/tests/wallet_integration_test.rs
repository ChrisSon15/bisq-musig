use std::fs;
use std::str::FromStr as _;

use base64::Engine as _;
use bdk_kyoto::bip157::tokio;
use bdk_kyoto::{FeeRate, TrustedPeer};
use bdk_wallet::bitcoin::{Address, Amount, Network};
use bdk_wallet::error::CreateTxError::OutputBelowDustLimit;
use bdk_wallet::psbt::PsbtUtils as _;
use bdk_wallet::{KeychainKind, SignOptions};
use chain::CBFScanner;
use rand::RngCore as _;
use rusqlite::Connection;
use secp::Scalar;
use tempfile::{TempDir, tempdir};
use testenv::TestEnv;
use wallet::bmp_wallet::*;
use wallet::password::Password;
use wallet::error::WalletErrorKind;
use wallet::persisted::DBStorage;
use wallet::utils::derive_key_from_password;

/// A password that follows the wallet's password rules, for tests that need some password.
const TEST_PASSWORD: &str = "Test-Passw0rd";

/// [`TEST_PASSWORD`] as a [`Password`], for creating wallets and setting passwords.
fn test_password() -> Password {
    TEST_PASSWORD
        .parse()
        .expect("TEST_PASSWORD follows the password rules")
}

fn new_private_key() -> Scalar {
    let mut seed: [u8; 32] = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    Scalar::from_slice(&seed).unwrap()
}

#[tokio::test]
async fn init_test() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    let chain = env.new_testchain()?;

    let mut wallet = BMPWallet::new(
        env.new_temp_path().into(),
        &test_password(),
        Network::Regtest,
    )?;
    let receive_amount = Amount::from_sat(100_000);

    let receiving_addr = wallet.next_unused_address(KeychainKind::External);

    env.fund_address(&receiving_addr, receive_amount)?;
    env.mine_block()?;

    wallet.sync_all(&chain).await?;

    assert_eq!(wallet.balance(), receive_amount);
    Ok(())
}

#[tokio::test]
async fn test_sync_with_imported_keys() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    let chain = env.new_testchain()?;

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

    wallet.sync_all(&chain).await?;
    assert_eq!(wallet.balance(), receive_amount + receive_amount);

    Ok(())
}

#[tokio::test]
async fn test_broadcast_transaction() -> anyhow::Result<()> {
    // This test broadcast a transaction created from main wallet balance only
    let mut env = TestEnv::new()?;
    let chain = env.new_testchain()?;

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

    wallet.sync_all(&chain).await?;

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
    wallet.sync_all(&chain).await?;

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
    let chain = env.new_testchain()?;

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

    wallet.sync_all(&chain).await?;

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
    wallet.sync_all(&chain).await?;

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
    let chain = env.new_testchain()?;

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

    wallet.sync_all(&chain).await?;

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
    wallet.sync_all(&chain).await?;

    let new_balance = (receive_amount + receive_amount) - send_amount - fee;
    assert_eq!(wallet.balance(), new_balance);

    // Reload the wallet to make sure the state changes are persisted
    let mut enc_wallet = BMPWallet::load_wallet(dir.into(), Network::Regtest, &test_password())?;

    env.fund_address(&main_wallet_addr, Amount::from_sat(10_000))?;
    env.mine_block()?;
    enc_wallet.sync_all(&chain).await?;
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
    wallet.sync_all(&CBFScanner::new(peers.to_vec())).await?;
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

    wallet.sync_all(&CBFScanner::new(peers)).await?;
    assert_eq!(wallet.balance(), Amount::from_sat(30_000));
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

    wallet.sync_all(&CBFScanner::new(peers)).await?;

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

    let cbf = CBFScanner::new(peers.to_vec());
    wallet.sync_all(&cbf).await?;
    assert_eq!(wallet.balance(), Amount::from_sat(230_000));

    // Reload the wallet from persisted state
    let mut loaded_wallet = BMPWallet::load_wallet(dir.into(), Network::Regtest, &test_password())?;
    assert_eq!(loaded_wallet.balance(), Amount::from_sat(230_000));

    env.fund_address(&addr, Amount::from_sat(70_000))?;
    env.mine_block()?;
    loaded_wallet.sync_all(&cbf).await?;
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

    loaded_wallet.sync_all(&cbf).await?;
    assert_eq!(loaded_wallet.balance(), Amount::from_sat(230_000) - fee);

    Ok(())
}

#[tokio::test]
async fn test_drain_wallet_with_main_balance() -> anyhow::Result<()> {
    // This test will attempt to drain the imported wallets UTXOs
    // With the main wallet having some balance it won't be touched.
    let mut env = TestEnv::new()?;
    let chain = env.new_testchain()?;

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
    wallet.sync_all(&chain).await?;

    // Doing *2 because we have two imported keys with the same amount received 10_000
    let current_balance = amount_to_send_main_wallet + amount_to_send_imported * 2;
    assert_eq!(wallet.balance(), current_balance);

    let mut psbt = wallet.drain_imported_balance(FeeRate::from_sat_per_vb(10).unwrap())?;

    wallet.sign(&mut psbt, SignOptions::default())?;

    let drained_amount = amount_to_send_imported * 2 - psbt.fee()?;

    let tx = psbt.extract_tx()?;

    env.broadcast(&tx)?;
    env.mine_block()?;

    wallet.sync_all(&chain).await?;

    let main_wallet_balance = drained_amount + amount_to_send_main_wallet;

    assert_eq!(wallet.balance(), main_wallet_balance);

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
        wallet.get_seed_phrase()?
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
        wallet.get_seed_phrase()?,
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
    assert_eq!(reloaded.get_seed_phrase()?, seed);

    Ok(())
}

/// A leftover staged salt — from a password change that failed before the re-key — must
/// neither stop nor confuse a normal load, and must be cleaned up.
#[test]
fn load_ignores_and_cleans_a_stale_staged_salt() -> anyhow::Result<()> {
    let dir = get_dir();
    let seed = {
        let wallet = BMPWallet::new(dir.path().into(), &test_password(), Network::Regtest)?;
        wallet.get_seed_phrase()?
    };

    let staged_salt_path = dir.path().join(format!("{}.salt.new", BMPWallet::DB_NAME));
    fs::write(&staged_salt_path, "bm90LXRoZS1yZWFsLXNhbHQ=")?; // valid base64, wrong salt

    let wallet = BMPWallet::load_wallet(dir.path().into(), Network::Regtest, &test_password())?;
    assert_eq!(wallet.get_seed_phrase()?, seed);
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

        let seed = wallet.get_seed_phrase()?;
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
            wallet.get_seed_phrase()?,
            seed,
            "seed must survive the re-key"
        );
        seed
    };

    // The rotated salt and key must both have reached disk.
    {
        let reloaded = BMPWallet::load_wallet(dir.path().into(), Network::Regtest, &new_password)?;
        assert_eq!(reloaded.get_seed_phrase()?, seed);
    }

    let stale = BMPWallet::load_wallet(dir.path().into(), Network::Regtest, &test_password());
    assert!(
        stale.is_err() || stale.unwrap().get_seed_phrase().is_err(),
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
        wallet.get_seed_phrase()?
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
    assert_eq!(reloaded.get_seed_phrase()?, seed);

    Ok(())
}

#[test]
fn load_wallet_rejects_a_wrong_password() {
    let dir = get_dir();
    let dir = dir.path();

    let bmp_wallet = BMPWallet::new(dir.into(), &test_password(), Network::Regtest).unwrap();
    let seed = bmp_wallet.get_seed_phrase().unwrap();

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
