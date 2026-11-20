//! Smoke test of the `musigd` binary: it starts and serves the two services that bisq2 uses, the
//! trade service `musigrpc.Musig` and, with `--wallet-dir`, the wallet service `wallet.Wallet`.
//!
//! The trade service uses the wallet of the wallet service, so a trade can be created only after
//! the wallet is open.
//!
//! The lock times of the trades are configuration values of `musigd`. It reads them from the
//! environment and does not start with values outside the limits.
//!
//! The first tests need no chain: without `--wallet-peer` the wallet does not sync. The tests of
//! whole trades run two `musigd` processes against a regtest node, which they reach only through
//! its peer-to-peer port.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bdk_wallet::bitcoin::{Address, Amount, Transaction, consensus};
use futures_util::StreamExt as _;
use rpc::pb::bmp_wallet::wallet_client::WalletClient;
use rpc::pb::bmp_wallet::{
    CloseRequest, GetBalanceRequest, IsWalletReadyRequest, OpenOrCreateWalletRequest,
};
use rpc::pb::musigrpc::musig_client::MusigClient;
use rpc::pb::musigrpc::{
    ClosureKind, CustomCloseTradeRequest, CustomPayoutPsbtRequest, PublishWarningTxRequest, Role,
    TradeEvent, TradeEventType, TradeStateKind, TradeTxKind, TxConfirmationStatus,
};
use testenv::TestEnv;
use tonic::transport::Channel;
use tonic::{Code, Streaming};

use crate::common::musigd::{self, App, Launch, MusigdProcess, PASSWORD, WAIT, connect, free_port};
use crate::common::trade::{self, DepositConfirmations, FEE_RATE, SignedDeposit, Terms};

/// The code that these tests share with the blackbox tests.
mod common;

/// The longest time in which `musigd` ends when it cannot start.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn musigd_serves_trade_service_and_wallet_service() {
    let wallet_dir = tempfile::tempdir().expect("temporary directory");
    let port = free_port().expect("free loopback port");
    let musigd = Command::new(env!("CARGO_BIN_EXE_musigd"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--wallet-dir")
        .arg(wallet_dir.path())
        // Lock times from the environment, inside the limits.
        .env("BISQ_MUSIG_WARNING_LOCK_TIME", "20")
        .env("BISQ_MUSIG_CLAIM_LOCK_TIME", "10")
        .env("RUST_LOG", "off")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("musigd starts");
    let _musigd = MusigdProcess(musigd);
    let channel = connect(port).await.expect("musigd accepts a connection");

    let mut wallet_client = WalletClient::new(channel.clone());
    let mut musig_client = MusigClient::new(channel);
    let init_trade_request = trade::init_request("musigd-smoke-test", Role::BuyerAsTaker);

    let wallet_state = wallet_client
        .is_wallet_ready(IsWalletReadyRequest {})
        .await
        .expect("wallet.Wallet is served")
        .into_inner();
    assert!(
        !wallet_state.ready,
        "no wallet is open in a new wallet directory"
    );

    // The trade service uses the wallet of the wallet service. Without an open wallet it refuses
    // to start a trade.
    let status = musig_client
        .init_trade(init_trade_request.clone())
        .await
        .expect_err("musigrpc.Musig is served, but no wallet is open");
    assert_eq!(status.code(), Code::FailedPrecondition, "{status}");
    assert!(status.message().contains("no wallet is open"), "{status}");

    wallet_client
        .open_or_create_wallet(OpenOrCreateWalletRequest {
            password: PASSWORD.to_owned(),
        })
        .await
        .expect("the wallet is created");
    let key_shares = musig_client
        .init_trade(init_trade_request)
        .await
        .expect("the trade is created with a key of the wallet")
        .into_inner();
    assert_eq!(key_shares.buyer_output_pub_key_share.len(), 33);
    assert_eq!(key_shares.multisig_script_key.len(), 32);
}

/// The lock times are read from the environment: with a warning lock time outside the limits,
/// `musigd` reports the value and ends, before it serves a request.
#[test]
fn musigd_does_not_start_with_a_lock_time_outside_the_limits() {
    for (variable, value, expected) in [
        (
            "BISQ_MUSIG_WARNING_LOCK_TIME",
            "9",
            "warning lock time of 9 blocks",
        ),
        (
            "BISQ_MUSIG_CLAIM_LOCK_TIME",
            "1",
            "claim lock time of 1 blocks",
        ),
        ("BISQ_MUSIG_WARNING_LOCK_TIME", "soon", "invalid value"),
    ] {
        let musigd = Command::new(env!("CARGO_BIN_EXE_musigd"))
            .arg("--port")
            .arg(free_port().expect("free loopback port").to_string())
            .env(variable, value)
            .env("RUST_LOG", "off")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("musigd starts");
        let mut musigd = MusigdProcess(musigd);

        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let status = loop {
            if let Some(status) = musigd.0.try_wait().expect("process state") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "musigd keeps running with {variable}={value}"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(!status.success(), "{variable}={value}: {status}");
        let mut error_output = String::new();
        musigd
            .0
            .stderr
            .take()
            .expect("stderr is piped")
            .read_to_string(&mut error_output)
            .expect("readable error output");
        assert!(
            error_output.contains(expected),
            "{variable}={value}: {error_output}"
        );
    }
}

// ---- A trade of two daemons on a regtest chain ----

const FUNDING: u64 = 100_000_000;
const TRADE_AMOUNT: u64 = 5_000_000;
const BUYERS_SECURITY_DEPOSIT: u64 = 1_000_000;
const SELLERS_SECURITY_DEPOSIT: u64 = 1_200_000;
/// The terms of every trade of these tests.
const TERMS: Terms = Terms {
    trade_amount: Amount::from_sat(TRADE_AMOUNT),
    buyers_security_deposit: Amount::from_sat(BUYERS_SECURITY_DEPOSIT),
    sellers_security_deposit: Amount::from_sat(SELLERS_SECURITY_DEPOSIT),
};

/// One trader: a `musigd` process with its wallet directory, and clients of its two services.
struct Daemon {
    /// "buyer" or "seller". The log files of the processes of the trader are named after it.
    name: &'static str,
    /// `None` while the daemon is stopped.
    process: Option<MusigdProcess>,
    wallet_dir: tempfile::TempDir,
    wallet: WalletClient<Channel>,
    musig: MusigClient<Channel>,
    /// The deposit timeout of the daemon in blocks, if it is not the default.
    deposit_timeout: Option<u16>,
}

impl Daemon {
    /// Starts `musigd` with a new wallet. Its only connection to the chain is the peer-to-peer
    /// port of the node. `deposit_timeout` is given to every process of this trader, if it is
    /// set.
    async fn start_with_deposit_timeout(
        name: &'static str,
        env: &TestEnv,
        deposit_timeout: Option<u16>,
    ) -> Self {
        let wallet_dir = tempfile::tempdir().expect("temporary directory");
        let app = Self::launch(name, env, wallet_dir.path(), deposit_timeout, true).await;
        Self {
            name,
            process: Some(app.process),
            wallet_dir,
            wallet: app.wallet,
            musig: app.musig,
            deposit_timeout,
        }
    }

    /// Starts a `musigd` process on a wallet directory and opens the wallet, which creates it if
    /// the directory is empty. The lock times of the process are shorter than the defaults, so
    /// that a test mines fewer blocks. `first` tells whether this is the first process of the
    /// trader in the test.
    async fn launch(
        name: &str,
        env: &TestEnv,
        wallet_dir: &std::path::Path,
        deposit_timeout: Option<u16>,
        first: bool,
    ) -> App {
        let mut config = vec![
            ("BISQ_MUSIG_WARNING_LOCK_TIME", "20".to_owned()),
            ("BISQ_MUSIG_CLAIM_LOCK_TIME", "10".to_owned()),
        ];
        if let Some(deposit_timeout) = deposit_timeout {
            config.push(("BISQ_MUSIG_DEPOSIT_TIMEOUT", deposit_timeout.to_string()));
        }
        App::start(&Launch {
            trader: name,
            wallet_dir,
            peer: env.p2p_socket_addr().expect("peer address"),
            config: &config,
            first,
        })
        .await
        .expect("musigd starts and opens the wallet")
    }

    /// Ends the `musigd` process at once, as a crash or a loss of power does: the process gets
    /// no chance to finish what it is doing. The wallet directory stays.
    fn stop(&mut self) {
        self.process = None;
    }

    /// Starts a new `musigd` process on the wallet directory of this trader and opens the
    /// wallet, as bisq2 does at its start.
    async fn restart(&mut self, env: &TestEnv) {
        self.stop();
        let app = Self::launch(
            self.name,
            env,
            self.wallet_dir.path(),
            self.deposit_timeout,
            false,
        )
        .await;
        self.process = Some(app.process);
        self.wallet = app.wallet;
        self.musig = app.musig;
    }

    async fn new_address(&mut self) -> Address {
        musigd::new_address(&mut self.wallet)
            .await
            .expect("an address of the wallet")
    }

    async fn balance(&mut self) -> u64 {
        self.wallet
            .get_balance(GetBalanceRequest {})
            .await
            .expect("balance")
            .into_inner()
            .balance
    }

    /// Waits until the wallet, which syncs with compact block filters once per second, reports a
    /// balance for which `expected` holds.
    async fn wait_for_balance(&mut self, what: &str, expected: impl Fn(u64) -> bool) -> u64 {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let balance = self.balance().await;
            if expected(balance) {
                return balance;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{what}: the balance stays at {balance} sat"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

async fn next_status(events: &mut Streaming<TxConfirmationStatus>) -> Option<TxConfirmationStatus> {
    tokio::time::timeout(WAIT, events.next())
        .await
        .expect("an event in time")
        .map(|event| event.expect("event without error"))
}

/// Runs the setup of a trade in which the buyer is the taker: messages A to D. Both traders
/// sign the deposit transaction. One receiver, an address of the node, gets everything that a
/// redirect transaction can distribute.
async fn set_up_trade(
    buyer: &mut Daemon,
    seller: &mut Daemon,
    env: &TestEnv,
    trade_id: &str,
) -> anyhow::Result<SignedDeposit> {
    let redirect_receiver = env.new_address()?;
    let (sellers, buyers) = (&mut seller.musig, &mut buyer.musig);
    let deposit = trade::set_up(sellers, buyers, trade_id, &TERMS, redirect_receiver).await?;
    // The coins of both deposits are reserved.
    assert_eq!(buyer.balance().await, 0);
    assert_eq!(seller.balance().await, 0);
    Ok(deposit)
}

/// Starts the daemons of the buyer and of the seller and funds both wallets.
async fn funded_daemons(env: &mut TestEnv) -> anyhow::Result<(Daemon, Daemon)> {
    funded_daemons_with_deposit_timeout(env, None).await
}

/// As [`funded_daemons`], with a deposit timeout for both daemons.
async fn funded_daemons_with_deposit_timeout(
    env: &mut TestEnv,
    deposit_timeout: Option<u16>,
) -> anyhow::Result<(Daemon, Daemon)> {
    let mut buyer = Daemon::start_with_deposit_timeout("buyer", env, deposit_timeout).await;
    let mut seller = Daemon::start_with_deposit_timeout("seller", env, deposit_timeout).await;
    for daemon in [&mut buyer, &mut seller] {
        let address = daemon.new_address().await;
        env.fund_address(&address, Amount::from_sat(FUNDING))?;
    }
    env.mine_block()?;
    for daemon in [&mut buyer, &mut seller] {
        daemon
            .wait_for_balance("funding", |balance| balance == FUNDING)
            .await;
    }
    Ok((buyer, seller))
}

/// J11, A1, B1 and M1 through the real daemon: two `musigd` processes set up a trade, the taker
/// publishes the deposit transaction over the peer-to-peer network, both report its
/// confirmations, the trade is closed cooperatively, both wallets show the payout, and `Close`
/// drains the payout over the peer-to-peer network. The daemons have no access to the RPC
/// interface of a node (D2, F27); the test uses it to mine and to fund.
#[tokio::test(flavor = "multi_thread")]
async fn j11_trade_of_two_daemons_without_bitcoin_core_rpc() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    // The node needs mature coins. The blocks are mined before the daemons connect.
    env.mine_blocks(101)?;
    let (mut buyer, mut seller) = funded_daemons(&mut env).await?;

    let trade_id = "trade-1".to_owned();
    let deposit = set_up_trade(&mut buyer, &mut seller, &env, &trade_id).await?;
    let deposit_txid = deposit.txid;

    // The maker observes the deposit, and the taker publishes it over the peer-to-peer network.
    let DepositConfirmations {
        sellers: mut sellers_events,
        buyers: mut buyers_events,
    } = trade::publish_deposit(&mut seller.musig, &mut buyer.musig, &trade_id, deposit).await?;

    // The engine does not learn whether the node accepted the transaction, so the test mines
    // blocks until the first confirmation is reported. Then two more blocks follow.
    let mut first = None;
    for _ in 0..30 {
        env.mine_block()?;
        if let Ok(event) = tokio::time::timeout(Duration::from_secs(3), buyers_events.next()).await
        {
            first = event.transpose()?;
            break;
        }
    }
    let first = first.expect("the deposit transaction was confirmed");
    assert_eq!(first.num_confirmations, 1);
    let deposit_tx: Transaction = consensus::deserialize(&first.tx)?;
    assert_eq!(deposit_tx.compute_txid(), deposit_txid);
    let deposit_height = first.current_block_height;
    assert_eq!(u64::from(deposit_height), env.block_count()?);
    env.mine_blocks(2)?;
    // Blocks that arrive together can be reported together, so a number can be left out. The
    // numbers rise, the last event reports 3 confirmations, and then the stream ends (D6).
    let mut reported = vec![first.num_confirmations];
    while let Some(event) = next_status(&mut buyers_events).await {
        assert_eq!(
            event.current_block_height,
            deposit_height + event.num_confirmations - 1
        );
        reported.push(event.num_confirmations);
    }
    assert!(
        reported.is_sorted() && reported.last() == Some(&3),
        "{reported:?}"
    );
    // The maker, who did not publish, gets events with the transaction from the block.
    let mut reported = Vec::new();
    while let Some(event) = next_status(&mut sellers_events).await {
        let found: Transaction = consensus::deserialize(&event.tx)?;
        assert_eq!(found.compute_txid(), deposit_tx.compute_txid());
        reported.push(event.num_confirmations);
    }
    assert!(
        reported.is_sorted() && reported.last() == Some(&3),
        "{reported:?}"
    );

    trade::close_cooperatively(&mut seller.musig, &mut buyer.musig, &trade_id).await?;

    // Each wallet shows the payout of its trader: the buyer gained the trade amount, the seller
    // paid it, apart from the fee of the deposit transaction.
    let fee_limit = 10_000;
    let buyers_balance = buyer
        .wait_for_balance("buyer's payout", |balance| {
            (FUNDING + TRADE_AMOUNT - fee_limit..=FUNDING + TRADE_AMOUNT).contains(&balance)
        })
        .await;
    seller
        .wait_for_balance("seller's payout", |balance| {
            (FUNDING - TRADE_AMOUNT - fee_limit..=FUNDING - TRADE_AMOUNT).contains(&balance)
        })
        .await;

    // Close drains the payout to the seed wallet. The wallet sends the transaction over the
    // peer-to-peer network, and it confirms.
    let drain_tx_id = buyer
        .wallet
        .close(CloseRequest {})
        .await?
        .into_inner()
        .drain_tx_id
        .expect("the payout is drained");
    let payout = TRADE_AMOUNT + BUYERS_SECURITY_DEPOSIT;
    let mut drained = false;
    for _ in 0..30 {
        env.mine_block()?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        // After the confirmation the whole balance is in the seed wallet again, minus the fee
        // of the drain transaction.
        let balance = buyer.balance().await;
        if balance < buyers_balance && balance > buyers_balance - fee_limit {
            drained = true;
            break;
        }
    }
    assert!(
        drained,
        "the drain transaction {drain_tx_id} of {payout} sat confirms"
    );
    Ok(())
}

/// Mines blocks, one every two seconds at most, until the stream delivers an event for which
/// `expected` holds.
async fn mine_until_event(
    env: &mut TestEnv,
    events: &mut Streaming<TradeEvent>,
    what: &str,
    expected: impl Fn(&TradeEvent) -> bool,
) -> anyhow::Result<TradeEvent> {
    for _ in 0..40 {
        env.mine_block()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.next()).await {
            let event = event?;
            if expected(&event) {
                return Ok(event);
            }
        }
    }
    anyhow::bail!("no event: {what}")
}

/// Sets up a trade, publishes the deposit transaction, mines blocks until it has 3
/// confirmations, and exchanges the messages E and F: the seller has released. The buyer does
/// not send message G.
async fn confirm_deposit_and_release(
    env: &mut TestEnv,
    buyer: &mut Daemon,
    seller: &mut Daemon,
    trade_id: &str,
) -> anyhow::Result<()> {
    set_up_and_confirm_deposit(env, buyer, seller, trade_id).await?;
    trade::start_payment(&mut seller.musig, &mut buyer.musig, trade_id).await?;
    trade::release(&mut seller.musig, trade_id).await?;
    Ok(())
}

/// Sets up a trade, publishes the deposit transaction and mines blocks until it has 3
/// confirmations. Both traders are in the state `Deposit`.
async fn set_up_and_confirm_deposit(
    env: &mut TestEnv,
    buyer: &mut Daemon,
    seller: &mut Daemon,
    trade_id: &str,
) -> anyhow::Result<()> {
    let deposit = set_up_trade(buyer, seller, env, trade_id).await?;
    let (sellers, buyers) = (&mut seller.musig, &mut buyer.musig);
    let mut deposit_events = trade::publish_deposit(sellers, buyers, trade_id, deposit)
        .await?
        .buyers;
    // Blocks are mined until the stream of the deposit ends, at 3 confirmations.
    let mut confirmed = false;
    for _ in 0..30 {
        env.mine_block()?;
        match tokio::time::timeout(Duration::from_secs(2), deposit_events.next()).await {
            Ok(None) => {
                confirmed = true;
                break;
            }
            Ok(Some(event)) => drop(event?),
            Err(_) => {}
        }
    }
    assert!(confirmed, "the deposit transaction has 3 confirmations");
    Ok(())
}

/// Sends a request until the daemon accepts it. Directly after a start a daemon refuses a
/// request that needs the chain, until it has evaluated the blocks since the deposit signature
/// of the trade and has a peer. Such a request can be repeated.
async fn until_accepted<T, F, Fut>(what: &str, mut request: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
{
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        match request().await {
            Ok(response) => return response.into_inner(),
            Err(status)
                if matches!(status.code(), Code::FailedPrecondition | Code::Unavailable)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Err(status) => panic!("{what}: {status}"),
        }
    }
}

/// Waits until the daemon reports a state of the trade for which `expected` holds. With
/// `mine`, a block is mined before every look, one every second at most.
async fn wait_for_state(
    env: &mut TestEnv,
    daemon: &mut Daemon,
    trade_id: &str,
    mine: bool,
    what: &str,
    expected: impl Fn(&TradeEvent) -> bool,
) -> anyhow::Result<TradeEvent> {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if mine {
            env.mine_block()?;
        }
        for _ in 0..4 {
            let state = trade::trade_state(&mut daemon.musig, trade_id).await?;
            if expected(&state) {
                return Ok(state);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "{what}: the trade stays in {:?} {:?}",
            trade::trade_state(&mut daemon.musig, trade_id)
                .await?
                .state(),
            trade::trade_state(&mut daemon.musig, trade_id)
                .await?
                .closure_type
        );
    }
}

fn is_closed_as(closure: ClosureKind) -> impl Fn(&TradeEvent) -> bool {
    move |event| event.state() == TradeStateKind::TradeClosed && event.closure_type() == closure
}

/// C1, C2, C4 (D7) through the real daemon: the seller releases, and the buyer does not answer.
/// The seller's `musigd` publishes the swap transaction on its own before the buyer's warning
/// transaction becomes valid. The buyer's `musigd` reads the seller's key share from the
/// confirmed transaction, closes the trade and imports the payout. No request asks for any of
/// this, and both wallets show the result.
#[tokio::test(flavor = "multi_thread")]
async fn c4_forced_close_of_two_daemons() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    env.mine_blocks(101)?;
    let (mut buyer, mut seller) = funded_daemons(&mut env).await?;
    let trade_id = "trade-1".to_owned();
    confirm_deposit_and_release(&mut env, &mut buyer, &mut seller, &trade_id).await?;

    let mut buyers_events = trade::trade_events(&mut buyer.musig, &trade_id).await?;
    let mut sellers_events = trade::trade_events(&mut seller.musig, &trade_id).await?;

    // The warning lock time is 20 blocks and the release safety margin 6 blocks. At 14
    // confirmations of the deposit the seller's daemon sends the swap transaction.
    let sent = mine_until_event(&mut env, &mut sellers_events, "swap sent", |event| {
        event.r#type() == TradeEventType::TxSent
    })
    .await?;
    assert_eq!(sent.tx_kind(), TradeTxKind::SwapTx);
    assert_eq!(sent.state(), TradeStateKind::TradeClosed);
    assert_eq!(sent.closure_type(), ClosureKind::Forced);

    // The swap transaction is confirmed: the buyer's daemon closes the buyer's trade.
    let closed = mine_until_event(&mut env, &mut buyers_events, "buyer closed", |event| {
        event.r#type() == TradeEventType::State && event.state() == TradeStateKind::TradeClosed
    })
    .await?;
    assert_eq!(closed.closure_type(), ClosureKind::Forced);

    // The buyer's wallet holds the payout of the trade, and the seller's wallet received his
    // deposit through the swap transaction.
    let fee_limit = 10_000;
    buyer
        .wait_for_balance("buyer's payout", |balance| {
            (FUNDING + TRADE_AMOUNT - fee_limit..=FUNDING + TRADE_AMOUNT).contains(&balance)
        })
        .await;
    seller
        .wait_for_balance("seller's deposit", |balance| {
            (FUNDING - TRADE_AMOUNT - fee_limit..=FUNDING - TRADE_AMOUNT).contains(&balance)
        })
        .await;
    Ok(())
}

/// H2, H3, H5 through the real daemon: both `musigd` processes are stopped and started again
/// after the seller released. Their trades are loaded from the wallet databases, in the states
/// in which they were stopped. Then the buyer's `musigd` is stopped again, and the seller's
/// swap transaction is confirmed while it does not run. After its start the buyer's `musigd`
/// evaluates the blocks that it missed, reads the key share from the swap transaction, closes
/// the trade and imports the payout. The result is the result of a run without a stop.
#[tokio::test(flavor = "multi_thread")]
async fn h2_h5_restart_of_two_daemons_during_a_forced_close() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    env.mine_blocks(101)?;
    let (mut buyer, mut seller) = funded_daemons(&mut env).await?;
    let trade_id = "trade-1".to_owned();
    confirm_deposit_and_release(&mut env, &mut buyer, &mut seller, &trade_id).await?;

    buyer.restart(&env).await;
    seller.restart(&env).await;
    let buyers_state = trade::trade_state(&mut buyer.musig, &trade_id).await?;
    assert_eq!(buyers_state.state(), TradeStateKind::BuyerReadyToRelease);
    let sellers_state = trade::trade_state(&mut seller.musig, &trade_id).await?;
    assert_eq!(sellers_state.state(), TradeStateKind::SellerReadyToRelease);
    // H3: the seller's caller repeats the release after the restart and gets the key share
    // again.
    let repeated = until_accepted("release", || {
        let mut musig = seller.musig.clone();
        let request = trade::swap_request(&trade_id, Vec::new(), true);
        async move { musig.sign_swap_tx(request).await }
    })
    .await;
    assert!(repeated.peer_output_prv_key_share.is_some());
    // K2: the ID of the trade is known after the restart.
    let duplicate = buyer
        .musig
        .init_trade(trade::init_request(&trade_id, Role::BuyerAsTaker))
        .await
        .expect_err("the trade ID is used");
    assert_eq!(duplicate.code(), Code::AlreadyExists);

    // The buyer's daemon does not run while the seller's daemon closes by force.
    buyer.stop();
    let mut sellers_events = trade::trade_events(&mut seller.musig, &trade_id).await?;
    mine_until_event(&mut env, &mut sellers_events, "swap sent", |event| {
        event.r#type() == TradeEventType::TxSent && event.tx_kind() == TradeTxKind::SwapTx
    })
    .await?;
    // The swap transaction is confirmed, with some blocks on top of it.
    env.mine_blocks(4)?;

    buyer.restart(&env).await;
    let mut buyers_events = trade::trade_events(&mut buyer.musig, &trade_id).await?;
    let deadline = tokio::time::Instant::now() + WAIT;
    let closed = loop {
        let event = tokio::time::timeout_at(deadline, buyers_events.next())
            .await
            .expect("the buyer's daemon finds the swap transaction in time")
            .expect("the stream is open")?;
        if event.r#type() == TradeEventType::State && event.state() == TradeStateKind::TradeClosed {
            break event;
        }
    };
    assert_eq!(closed.closure_type(), ClosureKind::Forced);

    let fee_limit = 10_000;
    buyer
        .wait_for_balance("buyer's payout", |balance| {
            (FUNDING + TRADE_AMOUNT - fee_limit..=FUNDING + TRADE_AMOUNT).contains(&balance)
        })
        .await;
    seller
        .wait_for_balance("seller's deposit", |balance| {
            (FUNDING - TRADE_AMOUNT - fee_limit..=FUNDING - TRADE_AMOUNT).contains(&balance)
        })
        .await;
    Ok(())
}

/// The balance of a wallet is `expected`, less the fees of the trade.
async fn expect_balance(daemon: &mut Daemon, what: &str, expected: u64) {
    let fee_limit = 10_000;
    daemon
        .wait_for_balance(what, |balance| {
            (expected - fee_limit..=expected).contains(&balance)
        })
        .await;
}

/// Mines blocks until the deposit transaction, which has 3 confirmations, has as many
/// confirmations as the warning lock time of the daemons (20 blocks).
fn mine_until_the_warning_is_valid(env: &mut TestEnv) -> anyhow::Result<()> {
    env.mine_blocks(17)?;
    Ok(())
}

/// The buyer asks his daemon to publish his warning transaction.
async fn publish_warning(daemon: &Daemon, trade_id: &str) -> Vec<u8> {
    until_accepted("PublishWarningTx", || {
        let mut musig = daemon.musig.clone();
        let request = PublishWarningTxRequest {
            trade_id: trade_id.to_owned(),
        };
        async move { musig.publish_warning_tx(request).await }
    })
    .await
    .warning_tx
}

/// H2, H3, H5 through the real daemon, for the dispute path with a redirect:
/// - both daemons are stopped and started in the state `Deposit`;
/// - the buyer's daemon is stopped directly after it accepted the request to publish the warning
///   transaction. After its start it knows the decision, sends the warning, and answers the
///   repeated request with the same transaction;
/// - the seller's daemon does not run while the warning is confirmed. After its start it finds the
///   warning in the blocks that it missed and sends the redirect in time;
/// - after a further start both daemons report the closed trade again, from the chain.
#[tokio::test(flavor = "multi_thread")]
async fn h2_h5_restart_of_two_daemons_in_the_dispute_path() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    env.mine_blocks(101)?;
    let (mut buyer, mut seller) = funded_daemons(&mut env).await?;
    let trade_id = "trade-1".to_owned();
    set_up_and_confirm_deposit(&mut env, &mut buyer, &mut seller, &trade_id).await?;

    buyer.restart(&env).await;
    seller.restart(&env).await;
    for daemon in [&mut buyer, &mut seller] {
        let state = trade::trade_state(&mut daemon.musig, &trade_id).await?;
        assert_eq!(state.state(), TradeStateKind::Deposit);
    }
    mine_until_the_warning_is_valid(&mut env)?;

    let warning_tx = publish_warning(&buyer, &trade_id).await;
    buyer.restart(&env).await;
    let state = trade::trade_state(&mut buyer.musig, &trade_id).await?;
    assert_eq!(state.state(), TradeStateKind::BuyersWarning);
    assert_eq!(publish_warning(&buyer, &trade_id).await, warning_tx);

    // The seller's daemon does not run while the warning transaction is confirmed.
    seller.stop();
    let mut buyers_events = trade::trade_events(&mut buyer.musig, &trade_id).await?;
    let confirmed = mine_until_event(&mut env, &mut buyers_events, "warning confirmed", |event| {
        event.r#type() == TradeEventType::TxConfirmed && event.tx_kind() == TradeTxKind::WarningTx
    })
    .await?;
    let warning_txid = consensus::deserialize::<Transaction>(&warning_tx)?.compute_txid();
    assert_eq!(confirmed.tx_id, Some(warning_txid.to_string()));
    env.mine_block()?;

    // The claim lock time is 10 blocks. The seller's daemon starts in time, finds the warning
    // and answers with the redirect.
    seller.restart(&env).await;
    wait_for_state(
        &mut env,
        &mut seller,
        &trade_id,
        false,
        "warning found",
        |state| state.state() == TradeStateKind::BuyersWarning,
    )
    .await?;
    let redirect = is_closed_as(ClosureKind::SellersRedirect);
    wait_for_state(
        &mut env,
        &mut seller,
        &trade_id,
        true,
        "redirect",
        &redirect,
    )
    .await?;
    wait_for_state(
        &mut env, &mut buyer, &trade_id, false, "redirect", &redirect,
    )
    .await?;

    // The closed state is not stored. After a start it follows from the chain again.
    env.mine_blocks(3)?;
    buyer.restart(&env).await;
    seller.restart(&env).await;
    for daemon in [&mut buyer, &mut seller] {
        let closed = wait_for_state(&mut env, daemon, &trade_id, false, "final", |state| {
            redirect(state) && state.r#final
        })
        .await?;
        assert_eq!(closed.deadlines, vec![]);
    }
    // Both traders lost what they paid into the deposit. The redirect paid it to the receiver.
    expect_balance(&mut buyer, "buyer", FUNDING - BUYERS_SECURITY_DEPOSIT).await;
    // The seller also paid for the child that raised the fee of the redirect. The claim lock
    // time of these daemons is 10 blocks, so the fee schedule takes a step with every block,
    // and the child that got into a block can be one of a late step.
    let sellers_deposit = TRADE_AMOUNT + SELLERS_SECURITY_DEPOSIT;
    let highest_child_fee = 250_000;
    seller
        .wait_for_balance("seller", |balance| {
            let after_the_deposit = FUNDING - sellers_deposit;
            (after_the_deposit - highest_child_fee..=after_the_deposit).contains(&balance)
        })
        .await;
    Ok(())
}

/// H2, H5 and M6 through the real daemon, for the dispute path with a claim: the buyer's daemon
/// is stopped and started while it waits for the claim lock time of its confirmed warning. It
/// continues to wait from the same block and claims when the lock time has passed. The seller's
/// daemon does not run in all this time. After its start it finds the warning and the claim, and
/// reports the trade that the buyer closed with his claim.
#[tokio::test(flavor = "multi_thread")]
async fn h2_h5_m6_restart_of_two_daemons_before_a_claim() -> anyhow::Result<()> {
    let mut env = TestEnv::new()?;
    env.mine_blocks(101)?;
    let (mut buyer, mut seller) = funded_daemons(&mut env).await?;
    let trade_id = "trade-1".to_owned();
    set_up_and_confirm_deposit(&mut env, &mut buyer, &mut seller, &trade_id).await?;
    mine_until_the_warning_is_valid(&mut env)?;
    seller.stop();

    publish_warning(&buyer, &trade_id).await;
    let mut buyers_events = trade::trade_events(&mut buyer.musig, &trade_id).await?;
    let confirmed = mine_until_event(&mut env, &mut buyers_events, "warning confirmed", |event| {
        event.r#type() == TradeEventType::TxConfirmed && event.tx_kind() == TradeTxKind::WarningTx
    })
    .await?;
    let claim_deadline = confirmed.deadlines.clone();
    assert_eq!(claim_deadline.len(), 1, "the claim lock time");
    drop(buyers_events);

    buyer.restart(&env).await;
    let waiting = wait_for_state(&mut env, &mut buyer, &trade_id, false, "warning", |state| {
        state.state() == TradeStateKind::BuyersWarning && !state.deadlines.is_empty()
    })
    .await?;
    assert_eq!(
        waiting.deadlines, claim_deadline,
        "the deadline is the same"
    );

    let claim = is_closed_as(ClosureKind::BuyersClaim);
    let closed = wait_for_state(&mut env, &mut buyer, &trade_id, true, "claim", &claim).await?;
    assert!(
        closed.current_block_height > claim_deadline[0].block_height,
        "the claim is confirmed after the claim lock time"
    );

    seller.restart(&env).await;
    wait_for_state(
        &mut env,
        &mut seller,
        &trade_id,
        false,
        "claim found",
        &claim,
    )
    .await?;
    // The buyer holds all funds of the trade. The seller lost his deposit.
    let sellers_deposit = TRADE_AMOUNT + SELLERS_SECURITY_DEPOSIT;
    expect_balance(&mut buyer, "buyer", FUNDING + sellers_deposit).await;
    expect_balance(&mut seller, "seller", FUNDING - sellers_deposit).await;
    Ok(())
}

/// H2, H3, H5 through the real daemon, for the custom payout:
/// - both daemons are stopped and started after they signed the custom payout. A repeated request
///   returns the same PSBT;
/// - the seller's daemon is stopped directly after it finished the transaction. After its start it
///   sends the transaction;
/// - the buyer's daemon does not run while the transaction is confirmed, and the buyer never
///   finishes it himself. After its start the daemon finds the transaction and closes the trade.
#[tokio::test(flavor = "multi_thread")]
async fn h2_h5_restart_of_two_daemons_during_a_custom_payout() -> anyhow::Result<()> {
    const SELLERS_AMOUNT: u64 = 3_000_000;
    let mut env = TestEnv::new()?;
    env.mine_blocks(101)?;
    let (mut buyer, mut seller) = funded_daemons(&mut env).await?;
    let trade_id = "trade-1".to_owned();
    set_up_and_confirm_deposit(&mut env, &mut buyer, &mut seller, &trade_id).await?;

    let sign = |daemon: &Daemon| {
        let musig = daemon.musig.clone();
        let request = CustomPayoutPsbtRequest {
            trade_id: trade_id.clone(),
            sellers_payout_amount_excluding_fee: SELLERS_AMOUNT,
            fee_rate: FEE_RATE.to_sat_per_kwu(),
        };
        until_accepted("SignCustomPayoutTx", move || {
            let (mut musig, request) = (musig.clone(), request.clone());
            async move { musig.sign_custom_payout_tx(request).await }
        })
    };
    let buyers_psbt = sign(&buyer).await;
    let sellers_psbt = sign(&seller).await;
    assert_eq!(buyers_psbt.tx_id, sellers_psbt.tx_id);

    buyer.restart(&env).await;
    seller.restart(&env).await;
    for daemon in [&mut buyer, &mut seller] {
        let state = trade::trade_state(&mut daemon.musig, &trade_id).await?;
        assert_eq!(state.state(), TradeStateKind::CustomPayoutSigned);
    }
    assert_eq!(sign(&buyer).await, buyers_psbt);
    assert_eq!(sign(&seller).await, sellers_psbt);

    buyer.stop();
    let finished = until_accepted("CustomCloseTrade", || {
        let mut musig = seller.musig.clone();
        let request = CustomCloseTradeRequest {
            trade_id: trade_id.clone(),
            peers_custom_payout_psbt: buyers_psbt.psbt.clone(),
        };
        async move { musig.custom_close_trade(request).await }
    })
    .await;
    let custom_payout_tx: Transaction = consensus::deserialize(&finished.custom_payout_tx)?;
    assert_eq!(
        custom_payout_tx.compute_txid().to_string(),
        sellers_psbt.tx_id
    );
    seller.restart(&env).await;

    let custom = is_closed_as(ClosureKind::Custom);
    let closed = wait_for_state(&mut env, &mut seller, &trade_id, true, "payout", |state| {
        custom(state) && state.r#final
    })
    .await?;
    assert!(closed.r#final);

    buyer.restart(&env).await;
    wait_for_state(
        &mut env,
        &mut buyer,
        &trade_id,
        false,
        "payout found",
        &custom,
    )
    .await?;
    let sellers_loss = TRADE_AMOUNT + SELLERS_SECURITY_DEPOSIT - SELLERS_AMOUNT;
    expect_balance(&mut buyer, "buyer", FUNDING + sellers_loss).await;
    expect_balance(&mut seller, "seller", FUNDING - sellers_loss).await;
    Ok(())
}

/// A3, H2 and H5 through the real daemon: both traders sign the deposit transaction, and nobody
/// publishes it. No daemon runs while the deposit timeout passes. The seller's daemon starts
/// first. It counts from the stored height, so it aborts the deposit at once, and its abort is
/// confirmed. The buyer's daemon starts later, finds the abort of the seller in the blocks that
/// it missed, and closes its trade without a transaction of its own. Both wallets hold their
/// coins again.
#[tokio::test(flavor = "multi_thread")]
async fn a3_h2_deposit_abort_of_two_daemons_after_a_restart() -> anyhow::Result<()> {
    const DEPOSIT_TIMEOUT: u16 = 8;
    let mut env = TestEnv::new()?;
    env.mine_blocks(101)?;
    let (mut buyer, mut seller) =
        funded_daemons_with_deposit_timeout(&mut env, Some(DEPOSIT_TIMEOUT)).await?;
    let trade_id = "trade-1".to_owned();
    set_up_trade(&mut buyer, &mut seller, &env, &trade_id).await?;
    let signed = trade::trade_state(&mut seller.musig, &trade_id).await?;
    assert_eq!(signed.state(), TradeStateKind::Deposit);
    assert_eq!(signed.deadlines.len(), 1, "the deposit timeout");
    let timeout_height = signed.deadlines[0].block_height;
    assert_eq!(
        timeout_height,
        signed.current_block_height + u32::from(DEPOSIT_TIMEOUT) + 1
    );

    buyer.stop();
    seller.stop();
    env.mine_blocks(usize::from(DEPOSIT_TIMEOUT) + 1)?;
    seller.restart(&env).await;
    let state = trade::trade_state(&mut seller.musig, &trade_id).await?;
    assert_eq!(state.deadlines[0].block_height, timeout_height);

    let aborted = is_closed_as(ClosureKind::DepositAborted);
    wait_for_state(&mut env, &mut seller, &trade_id, true, "abort", &aborted).await?;
    let fee_limit = 10_000;
    seller
        .wait_for_balance("seller's coins", |balance| {
            (FUNDING - fee_limit..FUNDING).contains(&balance)
        })
        .await;

    buyer.restart(&env).await;
    wait_for_state(
        &mut env,
        &mut buyer,
        &trade_id,
        false,
        "abort found",
        &aborted,
    )
    .await?;
    buyer
        .wait_for_balance("buyer's coins", |balance| balance == FUNDING)
        .await;
    Ok(())
}

/// Sends a request and ends the daemon at once, `delay` after the request was sent: before the
/// daemon got it, while the daemon works on it, or after the response left. Returns the
/// response if it arrived.
async fn kill_during<T, Fut>(daemon: &mut Daemon, delay: Duration, request: Fut) -> Option<T>
where
    T: Send + 'static,
    Fut: std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>> + Send + 'static,
{
    let pending = tokio::spawn(request);
    tokio::time::sleep(delay).await;
    daemon.stop();
    pending
        .await
        .expect("the request ends")
        .ok()
        .map(tonic::Response::into_inner)
}

/// The buyer's daemon is ended while it selects and reserves the coins of the deposit of a
/// new trade. After its start the trade is abandoned, the coins are free again, and the trade
/// ID stays used.
async fn interrupt_a_setup(
    env: &TestEnv,
    buyer: &mut Daemon,
    seller: &mut Daemon,
    delay: Duration,
) -> anyhow::Result<()> {
    let interrupted = "trade-0".to_owned();
    let init = |my_role: Role| trade::init_request(&interrupted, my_role);
    buyer.musig.init_trade(init(Role::BuyerAsTaker)).await?;
    let sellers_keys = seller
        .musig
        .init_trade(init(Role::SellerAsMaker))
        .await?
        .into_inner();
    let nonce_request = trade::nonce_request(&interrupted, &TERMS, sellers_keys);
    let mut musig = buyer.musig.clone();
    kill_during(buyer, delay, async move {
        musig.get_nonce_shares(nonce_request).await
    })
    .await;
    buyer.restart(env).await;
    let state = trade::trade_state(&mut buyer.musig, &interrupted).await?;
    assert_eq!(state.state(), TradeStateKind::TradeClosed);
    assert_eq!(state.closure_type(), ClosureKind::Abandoned);
    buyer
        .wait_for_balance("coins of the interrupted setup", |balance| {
            balance == FUNDING
        })
        .await;
    let duplicate = buyer
        .musig
        .init_trade(init(Role::BuyerAsTaker))
        .await
        .expect_err("the trade ID is used");
    assert_eq!(duplicate.code(), Code::AlreadyExists);
    Ok(())
}

/// H3, H4 and H8 through the real daemon: `musigd` is ended at once while it processes a
/// request, for one request of the setup and for every request of a cooperative close. The
/// moment differs from request to request: the daemon can have stored the new state or not, and
/// the response can have left or not. In every case:
/// - a setup that was interrupted is abandoned after the start, its coins are free again and its
///   trade ID stays used;
/// - a request after the deposit that is sent again after the start is accepted and returns what
///   the first response contained, if that one arrived;
/// - the trade ends as a trade without a stop, and both wallets hold their payouts.
#[tokio::test(flavor = "multi_thread")]
async fn h3_h4_h8_daemon_is_ended_while_it_processes_a_request() -> anyhow::Result<()> {
    let delays = [1, 25, 4, 60, 10, 2].map(Duration::from_millis);
    let mut env = TestEnv::new()?;
    env.mine_blocks(101)?;
    let (mut buyer, mut seller) = funded_daemons(&mut env).await?;

    interrupt_a_setup(&env, &mut buyer, &mut seller, delays[0]).await?;

    // A new trade of the same daemons, up to the confirmed deposit.
    let trade_id = "trade-1".to_owned();
    set_up_and_confirm_deposit(&mut env, &mut buyer, &mut seller, &trade_id).await?;

    // Message E: the buyer starts the payment.
    let start_payment = trade::start_payment_request(&trade_id);
    let (mut musig, request) = (buyer.musig.clone(), start_payment.clone());
    let first = kill_during(&mut buyer, delays[1], async move {
        musig.get_partial_signatures(request).await
    })
    .await;
    buyer.restart(&env).await;
    let message_e = until_accepted("message E", || {
        let (mut musig, request) = (buyer.musig.clone(), start_payment.clone());
        async move { musig.get_partial_signatures(request).await }
    })
    .await;
    if let Some(first) = first {
        assert_eq!(first, message_e);
    }
    let buyers_swap_signature = message_e
        .swap_tx_input_partial_signature
        .expect("message E contains the signature");

    // The seller takes message E, and releases: message F.
    let mut message_f = None;
    for (delay, request) in [
        (
            delays[2],
            trade::swap_request(&trade_id, buyers_swap_signature, false),
        ),
        (delays[3], trade::swap_request(&trade_id, Vec::new(), true)),
    ] {
        let (mut musig, first_request) = (seller.musig.clone(), request.clone());
        let first = kill_during(&mut seller, delay, async move {
            musig.sign_swap_tx(first_request).await
        })
        .await;
        seller.restart(&env).await;
        let response = until_accepted("SignSwapTx", || {
            let (mut musig, request) = (seller.musig.clone(), request.clone());
            async move { musig.sign_swap_tx(request).await }
        })
        .await;
        if let Some(first) = first {
            assert_eq!(first, response);
        }
        message_f = response.peer_output_prv_key_share;
    }
    assert!(message_f.is_some(), "message F contains the key share");

    // The buyer closes with message F and returns message G. The seller closes with message G.
    let mut key_share = message_f;
    for (delay, is_buyer) in [(delays[4], true), (delays[5], false)] {
        let daemon = if is_buyer { &mut buyer } else { &mut seller };
        let request = trade::close_request(&trade_id, key_share.clone());
        let (mut musig, first_request) = (daemon.musig.clone(), request.clone());
        let first = kill_during(daemon, delay, async move {
            musig.close_trade(first_request).await
        })
        .await;
        daemon.restart(&env).await;
        let response = until_accepted("CloseTrade", || {
            let (mut musig, request) = (daemon.musig.clone(), request.clone());
            async move { musig.close_trade(request).await }
        })
        .await;
        if let Some(first) = first {
            assert_eq!(first, response);
        }
        if is_buyer {
            assert!(response.peer_output_prv_key_share.is_some(), "message G");
        }
        key_share = response.peer_output_prv_key_share;
    }

    for daemon in [&mut buyer, &mut seller] {
        let state = trade::trade_state(&mut daemon.musig, &trade_id).await?;
        assert_eq!(state.state(), TradeStateKind::TradeClosed);
        assert_eq!(state.closure_type(), ClosureKind::Cooperative);
    }
    expect_balance(&mut buyer, "buyer's payout", FUNDING + TRADE_AMOUNT).await;
    expect_balance(&mut seller, "seller's payout", FUNDING - TRADE_AMOUNT).await;
    Ok(())
}
