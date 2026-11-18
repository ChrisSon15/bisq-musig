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
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bdk_wallet::bitcoin::{Address, Amount, FeeRate, Network, Transaction, consensus};
use futures_util::StreamExt as _;
use protocol::receiver::Receiver;
use rpc::pb::bmp_wallet::wallet_client::WalletClient;
use rpc::pb::bmp_wallet::{
    CloseRequest, GetBalanceRequest, GetUnusedAddressRequest, IsWalletReadyRequest,
    OpenOrCreateWalletRequest,
};
use rpc::pb::musigrpc::musig_client::MusigClient;
use rpc::pb::musigrpc::{
    CloseTradeRequest, ClosureKind, DepositPsbt, DepositTxSignatureRequest, NonceSharesMessage,
    NonceSharesRequest, PartialSignaturesMessage, PartialSignaturesRequest, PubKeySharesRequest,
    PubKeySharesResponse, PublishDepositTxRequest, ReceiverAddressAndAmount, Role,
    SubscribeTradeEventsRequest, SubscribeTxConfirmationStatusRequest, SwapTxSignatureRequest,
    TradeEvent, TradeEventType, TradeStateKind, TradeTxKind, TxConfirmationStatus,
};
use testenv::TestEnv;
use tonic::transport::Channel;
use tonic::{Code, Streaming};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Stops the `musigd` process when the test ends, also if the test fails.
struct MusigdProcess(Child);

impl Drop for MusigdProcess {
    fn drop(&mut self) {
        // Errors are ignored, because the process may have ended already.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Returns a loopback port that was free a moment ago.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("free loopback port");
    listener
        .local_addr()
        .expect("listener has a local address")
        .port()
}

/// Connects to `musigd` on the loopback address, and retries while the process starts.
async fn connect(port: u16) -> Channel {
    let endpoint = Channel::from_shared(format!("http://127.0.0.1:{port}")).expect("valid URI");
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        match endpoint.connect().await {
            Ok(channel) => return channel,
            Err(error) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "musigd did not accept a connection: {error}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

#[tokio::test]
async fn musigd_serves_trade_service_and_wallet_service() {
    let wallet_dir = tempfile::tempdir().expect("temporary directory");
    let port = free_port();
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
    let channel = connect(port).await;

    let mut wallet_client = WalletClient::new(channel.clone());
    let mut musig_client = MusigClient::new(channel);
    let init_trade_request = PubKeySharesRequest {
        trade_id: "musigd-smoke-test".to_owned(),
        my_role: Role::BuyerAsTaker.into(),
    };

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
            password: "S3cret!!".to_owned(),
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
            .arg(free_port().to_string())
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

const PASSWORD: &str = "S3cret!!";
const FUNDING: u64 = 100_000_000;
const TRADE_AMOUNT: u64 = 5_000_000;
const BUYERS_SECURITY_DEPOSIT: u64 = 1_000_000;
const SELLERS_SECURITY_DEPOSIT: u64 = 1_200_000;
/// 10 sat per vbyte.
const FEE_RATE: u64 = 2_500;
const WAIT: Duration = Duration::from_secs(90);

/// One trader: a `musigd` process with its wallet directory, and clients of its two services.
struct Daemon {
    /// `None` while the daemon is stopped.
    process: Option<MusigdProcess>,
    wallet_dir: tempfile::TempDir,
    wallet: WalletClient<Channel>,
    musig: MusigClient<Channel>,
}

impl Daemon {
    /// Starts `musigd` with a new wallet. Its only connection to the chain is the peer-to-peer
    /// port of the node: no option for the RPC interface of a node exists.
    async fn start(env: &TestEnv) -> Self {
        let wallet_dir = tempfile::tempdir().expect("temporary directory");
        let (process, wallet, musig) = Self::launch(env, wallet_dir.path()).await;
        Self {
            process: Some(process),
            wallet_dir,
            wallet,
            musig,
        }
    }

    /// Starts a `musigd` process on a wallet directory and opens the wallet, which creates it if
    /// the directory is empty.
    async fn launch(
        env: &TestEnv,
        wallet_dir: &std::path::Path,
    ) -> (MusigdProcess, WalletClient<Channel>, MusigClient<Channel>) {
        let port = free_port();
        let peer = env.p2p_socket_addr().expect("peer address");
        // For the search for an error: with the directory `MUSIGD_TEST_LOG_DIR` in the
        // environment, every daemon writes its log to a file there, named after its port.
        let log_dir = std::env::var_os("MUSIGD_TEST_LOG_DIR");
        let log_level = if log_dir.is_some() { "debug" } else { "off" };
        let log_file = |port: u16, stream: &str| match &log_dir {
            Some(dir) => {
                let path = std::path::Path::new(dir).join(format!("musigd-{port}.{stream}"));
                Stdio::from(std::fs::File::create(path).expect("log file"))
            }
            None => Stdio::null(),
        };
        let process = Command::new(env!("CARGO_BIN_EXE_musigd"))
            .arg("--port")
            .arg(port.to_string())
            .arg("--wallet-dir")
            .arg(wallet_dir)
            .arg("--wallet-peer")
            .arg(peer.to_string())
            .arg("--wallet-poll-secs")
            .arg("1")
            .env("BISQ_MUSIG_WARNING_LOCK_TIME", "20")
            .env("BISQ_MUSIG_CLAIM_LOCK_TIME", "10")
            .env("RUST_LOG", log_level)
            .stdout(log_file(port, "out"))
            .stderr(log_file(port, "err"))
            .spawn()
            .expect("musigd starts");
        let process = MusigdProcess(process);
        let channel = connect(port).await;
        let mut wallet = WalletClient::new(channel.clone());
        wallet
            .open_or_create_wallet(OpenOrCreateWalletRequest {
                password: PASSWORD.to_owned(),
            })
            .await
            .expect("the wallet is opened");
        (process, wallet, MusigClient::new(channel))
    }

    /// Ends the `musigd` process. The wallet directory stays.
    fn stop(&mut self) {
        self.process = None;
    }

    /// Starts a new `musigd` process on the wallet directory of this trader and opens the
    /// wallet, as bisq2 does at its start.
    async fn restart(&mut self, env: &TestEnv) {
        self.stop();
        let (process, wallet, musig) = Self::launch(env, self.wallet_dir.path()).await;
        self.process = Some(process);
        self.wallet = wallet;
        self.musig = musig;
    }

    async fn new_address(&mut self) -> Address {
        let address = self
            .wallet
            .get_unused_address(GetUnusedAddressRequest {})
            .await
            .expect("an address")
            .into_inner()
            .address
            .expect("address is set")
            .address;
        address
            .parse::<Address<_>>()
            .expect("valid address")
            .require_network(Network::Regtest)
            .expect("regtest address")
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
/// sign the deposit transaction. Returns the deposit PSBT with the signatures of the seller.
async fn set_up_trade(
    buyer: &mut Daemon,
    seller: &mut Daemon,
    env: &TestEnv,
    trade_id: &str,
) -> anyhow::Result<DepositPsbt> {
    let init = |my_role: Role| PubKeySharesRequest {
        trade_id: trade_id.to_owned(),
        my_role: my_role.into(),
    };
    let buyers_keys = buyer
        .musig
        .init_trade(init(Role::BuyerAsTaker))
        .await?
        .into_inner();
    let sellers_keys = seller
        .musig
        .init_trade(init(Role::SellerAsMaker))
        .await?
        .into_inner();
    let nonce_request = |peers_keys: &PubKeySharesResponse| NonceSharesRequest {
        trade_id: trade_id.to_owned(),
        buyer_output_peers_pub_key_share: peers_keys.buyer_output_pub_key_share.clone(),
        seller_output_peers_pub_key_share: peers_keys.seller_output_pub_key_share.clone(),
        peers_multisig_script_key: peers_keys.multisig_script_key.clone(),
        deposit_tx_fee_rate: FEE_RATE,
        prepared_tx_fee_rate: FEE_RATE,
        trade_amount: TRADE_AMOUNT,
        buyers_security_deposit: BUYERS_SECURITY_DEPOSIT,
        sellers_security_deposit: SELLERS_SECURITY_DEPOSIT,
        trade_fee_receiver: None,
    };
    let sellers_nonces = seller
        .musig
        .get_nonce_shares(nonce_request(&buyers_keys))
        .await?
        .into_inner();
    let buyers_nonces = buyer
        .musig
        .get_nonce_shares(nonce_request(&sellers_keys))
        .await?
        .into_inner();
    // The coins of both deposits are reserved now.
    assert_eq!(buyer.balance().await, 0);
    assert_eq!(seller.balance().await, 0);

    // One receiver gets everything that a redirect transaction can distribute.
    assert_eq!(
        buyers_nonces.redirection_amount_msat,
        sellers_nonces.redirection_amount_msat
    );
    let receivers = Receiver::compute_receivers_from_shares(
        vec![(env.new_address()?, 1.0)],
        buyers_nonces.redirection_amount_msat,
        FeeRate::from_sat_per_kwu(FEE_RATE),
    )
    .expect("one receiver");
    let redirection_receivers = vec![ReceiverAddressAndAmount {
        address: receivers[0].address.to_string(),
        amount: receivers[0].amount.to_sat(),
    }];
    let signatures_request = |peers_nonces: &NonceSharesMessage| PartialSignaturesRequest {
        trade_id: trade_id.to_owned(),
        peers_nonce_shares: Some(peers_nonces.clone()),
        redirection_receivers: redirection_receivers.clone(),
        buyer_ready_to_release: false,
    };
    let buyers_signatures = buyer
        .musig
        .get_partial_signatures(signatures_request(&sellers_nonces))
        .await?
        .into_inner();
    let sellers_signatures = seller
        .musig
        .get_partial_signatures(signatures_request(&buyers_nonces))
        .await?
        .into_inner();
    let deposit_request = |peers_signatures: &PartialSignaturesMessage| DepositTxSignatureRequest {
        trade_id: trade_id.to_owned(),
        peers_partial_signatures: Some(peers_signatures.clone()),
    };
    let sellers_deposit_psbt = seller
        .musig
        .sign_deposit_tx(deposit_request(&buyers_signatures))
        .await?
        .into_inner();
    buyer
        .musig
        .sign_deposit_tx(deposit_request(&sellers_signatures))
        .await?;

    Ok(sellers_deposit_psbt)
}

/// Closes the trade cooperatively: messages E, F and G.
async fn close_cooperatively(
    buyer: &mut Daemon,
    seller: &mut Daemon,
    trade_id: &str,
) -> anyhow::Result<()> {
    let message_e = buyer
        .musig
        .get_partial_signatures(PartialSignaturesRequest {
            trade_id: trade_id.to_owned(),
            buyer_ready_to_release: true,
            ..PartialSignaturesRequest::default()
        })
        .await?
        .into_inner();
    let swap_request = |signature: Vec<u8>, seller_ready_to_release| SwapTxSignatureRequest {
        trade_id: trade_id.to_owned(),
        swap_tx_input_peers_partial_signature: signature,
        seller_ready_to_release,
    };
    let buyers_swap_signature = message_e
        .swap_tx_input_partial_signature
        .expect("message E contains the signature");
    seller
        .musig
        .sign_swap_tx(swap_request(buyers_swap_signature, false))
        .await?;
    let message_f = seller
        .musig
        .sign_swap_tx(swap_request(Vec::new(), true))
        .await?
        .into_inner();
    let close_request = |key_share: Option<Vec<u8>>| CloseTradeRequest {
        trade_id: trade_id.to_owned(),
        my_output_peers_prv_key_share: key_share,
        ..CloseTradeRequest::default()
    };
    let message_g = buyer
        .musig
        .close_trade(close_request(message_f.peer_output_prv_key_share))
        .await?
        .into_inner();
    seller
        .musig
        .close_trade(close_request(message_g.peer_output_prv_key_share))
        .await?;

    Ok(())
}

/// Starts the daemons of the buyer and of the seller and funds both wallets.
async fn funded_daemons(env: &mut TestEnv) -> anyhow::Result<(Daemon, Daemon)> {
    let mut buyer = Daemon::start(env).await;
    let mut seller = Daemon::start(env).await;
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
    let sellers_deposit_psbt = set_up_trade(&mut buyer, &mut seller, &env, &trade_id).await?;

    // The maker observes the deposit, and the taker publishes it over the peer-to-peer network.
    let mut sellers_events = seller
        .musig
        .subscribe_tx_confirmation_status(SubscribeTxConfirmationStatusRequest {
            trade_id: trade_id.clone(),
        })
        .await?
        .into_inner();
    let mut buyers_events = buyer
        .musig
        .publish_deposit_tx(PublishDepositTxRequest {
            trade_id: trade_id.clone(),
            peers_deposit_psbt: Some(sellers_deposit_psbt),
        })
        .await?
        .into_inner();

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

    close_cooperatively(&mut buyer, &mut seller, &trade_id).await?;

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
    let sellers_deposit_psbt = set_up_trade(buyer, seller, env, trade_id).await?;
    let mut deposit_events = buyer
        .musig
        .publish_deposit_tx(PublishDepositTxRequest {
            trade_id: trade_id.to_owned(),
            peers_deposit_psbt: Some(sellers_deposit_psbt),
        })
        .await?
        .into_inner();
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

    let message_e = buyer
        .musig
        .get_partial_signatures(PartialSignaturesRequest {
            trade_id: trade_id.to_owned(),
            buyer_ready_to_release: true,
            ..PartialSignaturesRequest::default()
        })
        .await?
        .into_inner();
    let swap_request = |signature: Vec<u8>, seller_ready_to_release| SwapTxSignatureRequest {
        trade_id: trade_id.to_owned(),
        swap_tx_input_peers_partial_signature: signature,
        seller_ready_to_release,
    };
    let buyers_swap_signature = message_e
        .swap_tx_input_partial_signature
        .expect("message E contains the signature");
    seller
        .musig
        .sign_swap_tx(swap_request(buyers_swap_signature, false))
        .await?;
    seller
        .musig
        .sign_swap_tx(swap_request(Vec::new(), true))
        .await?;
    Ok(())
}

/// The event stream of a trade.
async fn trade_events(
    daemon: &mut Daemon,
    trade_id: &str,
) -> anyhow::Result<Streaming<TradeEvent>> {
    let request = SubscribeTradeEventsRequest {
        trade_id: trade_id.to_owned(),
    };
    Ok(daemon
        .musig
        .subscribe_trade_events(request)
        .await?
        .into_inner())
}

/// The first event of a new event stream of a trade: its current state.
async fn trade_state(daemon: &mut Daemon, trade_id: &str) -> anyhow::Result<TradeEvent> {
    let mut events = trade_events(daemon, trade_id).await?;
    let first = tokio::time::timeout(WAIT, events.next())
        .await?
        .expect("the stream starts with the state")?;
    assert_eq!(first.r#type(), TradeEventType::State);
    Ok(first)
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

    let mut buyers_events = trade_events(&mut buyer, &trade_id).await?;
    let mut sellers_events = trade_events(&mut seller, &trade_id).await?;

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
    let buyers_state = trade_state(&mut buyer, &trade_id).await?;
    assert_eq!(buyers_state.state(), TradeStateKind::BuyerReadyToRelease);
    let sellers_state = trade_state(&mut seller, &trade_id).await?;
    assert_eq!(sellers_state.state(), TradeStateKind::SellerReadyToRelease);
    // H3: the seller's caller repeats the release after the restart and gets the key share
    // again.
    let repeated = seller
        .musig
        .sign_swap_tx(SwapTxSignatureRequest {
            trade_id: trade_id.clone(),
            swap_tx_input_peers_partial_signature: Vec::new(),
            seller_ready_to_release: true,
        })
        .await?
        .into_inner();
    assert!(repeated.peer_output_prv_key_share.is_some());
    // K2: the ID of the trade is known after the restart.
    let duplicate = buyer
        .musig
        .init_trade(PubKeySharesRequest {
            trade_id: trade_id.clone(),
            my_role: Role::BuyerAsTaker.into(),
        })
        .await
        .expect_err("the trade ID is used");
    assert_eq!(duplicate.code(), Code::AlreadyExists);

    // The buyer's daemon does not run while the seller's daemon closes by force.
    buyer.stop();
    let mut sellers_events = trade_events(&mut seller, &trade_id).await?;
    mine_until_event(&mut env, &mut sellers_events, "swap sent", |event| {
        event.r#type() == TradeEventType::TxSent && event.tx_kind() == TradeTxKind::SwapTx
    })
    .await?;
    // The swap transaction is confirmed, with some blocks on top of it.
    env.mine_blocks(4)?;

    buyer.restart(&env).await;
    let mut buyers_events = trade_events(&mut buyer, &trade_id).await?;
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
