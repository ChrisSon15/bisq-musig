//! Characterisation tests for the `musigrpc.Musig` trade service (completion plan, section 9.5,
//! step 1; test level L3).
//!
//! Two in-process [`MusigImpl`] services stand for the two traders. Each one is driven through a
//! tonic client over a loopback port, and [`Trade`] carries the messages A to G of
//! `rpc/musig-trade-protocol-messages.txt` between the two sides. The call order follows
//! `rpc/src/main/java/bisq/TradeProtocolClient.java`.
//!
//! The services use the mock wallets of `protocol::mocks` and no chain. For this reason the tests
//! check states, returned data and errors, but no wallet balance and no confirmation.
//!
//! Each service has its own trade store and its own mock wallet, like the `musigd` of one trader.
//! The buyer and the seller of a trade use the same trade ID. A mock wallet has the coins, keys
//! and addresses for one trade, so every trade of a test starts two new services.
//!
//! The test IDs in the comments (B1, C1, K1, ...) refer to section 9.4 of
//! `concept/CompletionPlan.md`, the F numbers to its findings.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use bdk_wallet::bitcoin::hashes::Hash as _;
use bdk_wallet::bitcoin::secp256k1::{Message, Secp256k1};
use bdk_wallet::bitcoin::sighash::{Prevouts, SighashCache};
use bdk_wallet::bitcoin::transaction::Version;
use bdk_wallet::bitcoin::{
    Address, Amount, BlockHash, Network, OutPoint, Psbt, ScriptBuf, Sequence, TapSighashType,
    Transaction, TxIn, TxOut, Txid, XOnlyPublicKey, absolute, consensus, taproot,
};
use chain::client::{
    BroadcastFuture, ChainAccess, ChainEvent, ClientError, ConfirmedTx, PeerProblem,
};
use futures_util::StreamExt as _;
use musig2::secp::{Point, Scalar};
use protocol::mocks::{mock_buyer_trade_wallet, mock_seller_trade_wallet};
use protocol::params::EngineConfig;
use protocol::state::{ClosureType, TradeState};
use protocol::trade_model::{MemoryTradeStore, TradeModelStore as _};
use rpc::pb::musigrpc::musig_client::MusigClient;
use rpc::pb::musigrpc::{
    AbandonTradeRequest, AbandonTradeResponse, CloseTradeRequest, CloseTradeResponse, ClosureKind,
    ContractualTxIds, CustomCloseTradeRequest, CustomCloseTradeResponse, CustomPayoutPsbt,
    CustomPayoutPsbtRequest, DepositPsbt, DepositTxSignatureRequest, NonceSharesMessage,
    NonceSharesRequest, PartialSignaturesMessage, PartialSignaturesRequest, PubKeySharesRequest,
    PubKeySharesResponse, PublishDepositTxRequest, PublishWarningTxRequest,
    PublishWarningTxResponse, ReceiverAddressAndAmount, Role, SubscribeTradeEventsRequest,
    SubscribeTxConfirmationStatusRequest, SwapTxSignatureRequest, SwapTxSignatureResponse,
    TradeEvent, TradeEventType, TradeStateKind, TradeTxKind, TxConfirmationStatus,
};
use rpc::server::{MusigImpl, MusigServer, TradeWalletProvider, WalletOperation, restore_trades};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Server};
use tonic::{Code, Response, Status, Streaming};
use wallet::protocol_wallet_api::ProtocolWalletApi;

// Trade parameters of the Java test client. Fee rates are in sat per kwu, amounts in sat.
const DEPOSIT_TX_FEE_RATE: u64 = 3_125;
const PREPARED_TX_FEE_RATE: u64 = 2_500;
const TRADE_AMOUNT: u64 = 200_000;
const BUYERS_SECURITY_DEPOSIT: u64 = 30_000;
const SELLERS_SECURITY_DEPOSIT: u64 = 30_000;
const BUYERS_PENALTY_TX_FEE_RATE: u64 = 3_750;
const CUSTOM_PAYOUT_TX_FEE_RATE: u64 = 3_750;
// Lock times in blocks. The values have no meaning for these tests, because no chain is used.
/// The state of a trade that ended during the setup. A setup that the engine terminates is
/// abandoned at once, which releases the reserved coins.
const ABANDONED: TradeState = TradeState::TradeClosed(ClosureType::Abandoned);
const WARNING_LOCK_TIME: u16 = 20;
const CLAIM_LOCK_TIME: u16 = 10;

//noinspection SpellCheckingInspection
const TRADE_FEE_RECEIVER: (&str, u64) = ("bcrt1qwk6p86mzqmstcsg99qlu2mhsp3766u68jktv6k", 5_000);

// The costs of these receivers add up to `REDIRECTION_AMOUNT_MSAT`. The cost of a receiver is its
// amount plus the fee for its output at the prepared fee rate (43 vB for P2TR, 31 vB for P2WPKH,
// 32 vB for P2SH).
//noinspection SpellCheckingInspection
const REDIRECTION_RECEIVERS: [(&str, u64); 3] = [
    (
        "bcrt1phc8m8vansnl4utths947mjquprw20puwrrdfrwx8akeeu2tqwklsnxsvf0",
        160_000,
    ),
    ("bcrt1qwk6p86mzqmstcsg99qlu2mhsp3766u68jktv6k", 80_000),
    ("2N2x2bA28AsLZZEHss4SjFoyToQV5YYZsJM", 15_055),
];

// Weights of the signed transactions, as defined in `protocol::transaction`.
const WARNING_TX_WEIGHT: u64 = 846;
const FORWARDING_TX_WEIGHT: u64 = 444;
const CUSTOM_PAYOUT_TX_WEIGHT: u64 = 1_182;
const ANCHOR_AMOUNT: u64 = 330;

const DEPOSIT_AMOUNT: u64 = TRADE_AMOUNT + BUYERS_SECURITY_DEPOSIT + SELLERS_SECURITY_DEPOSIT;
const ESCROW_AMOUNT: u64 =
    DEPOSIT_AMOUNT - ANCHOR_AMOUNT - fee(PREPARED_TX_FEE_RATE, WARNING_TX_WEIGHT);
const REDIRECTION_AMOUNT_MSAT: u64 =
    (ESCROW_AMOUNT - ANCHOR_AMOUNT) * 1000 - PREPARED_TX_FEE_RATE * FORWARDING_TX_WEIGHT;

/// The fee in sat for the given fee rate in sat per kwu and weight in wu, rounded up.
const fn fee(fee_rate: u64, weight: u64) -> u64 {
    (fee_rate * weight).div_ceil(1000)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TradeType {
    TakerIsBuyer,
    TakerIsSeller,
}

const TRADE_TYPES: [TradeType; 2] = [TradeType::TakerIsBuyer, TradeType::TakerIsSeller];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Side {
    Buyer,
    Seller,
}

const SIDES: [Side; 2] = [Side::Buyer, Side::Seller];

/// How far a trade has progressed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    /// Messages A to C are exchanged. Nothing is signed. Both sides are in state `Init`.
    SignaturesExchanged,
    /// Both sides signed the deposit transaction and the taker published it.
    DepositSigned,
    /// The buyer started the payment and created message E. The seller did not receive it yet.
    BuyerStartedPayment,
    /// The seller received message E.
    PaymentStarted,
    /// The seller confirmed the payment and created message F. The buyer did not receive it yet.
    SellerReleased,
    /// The buyer closed with message F and created message G. The seller did not receive it yet.
    BuyerClosedCooperatively,
    /// Messages F and G are exchanged.
    ClosedCooperatively,
    /// The seller closed with the swap transaction. The buyer did not react yet.
    SellerClosedByForce,
    /// The seller closed with the swap transaction and the buyer read the key share from it.
    ClosedByForce,
    /// Both sides signed a custom payout directly after the deposit.
    CustomPayoutSigned,
    /// Both sides signed a custom payout after the seller received message E.
    CustomPayoutSignedAfterPaymentStart,
    /// Both sides finalised the custom payout transaction.
    ClosedByCustomPayout,
}

const STAGES: [Stage; 12] = [
    Stage::SignaturesExchanged,
    Stage::DepositSigned,
    Stage::BuyerStartedPayment,
    Stage::PaymentStarted,
    Stage::SellerReleased,
    Stage::BuyerClosedCooperatively,
    Stage::ClosedCooperatively,
    Stage::SellerClosedByForce,
    Stage::ClosedByForce,
    Stage::CustomPayoutSigned,
    Stage::CustomPayoutSignedAfterPaymentStart,
    Stage::ClosedByCustomPayout,
];

/// Gives a service the mock wallet of one trader.
struct MockWalletProvider(tokio::sync::Mutex<Box<dyn ProtocolWalletApi + Send>>);

#[tonic::async_trait]
impl TradeWalletProvider for MockWalletProvider {
    async fn with_trade_wallet(&self, operation: &mut WalletOperation<'_>) -> bool {
        operation(&mut **self.0.lock().await);
        true
    }
}

/// A chain without a network, for the service of one trader. The test decides which transactions
/// a new block contains.
struct FakeChain {
    state: Mutex<FakeChainState>,
    events: broadcast::Sender<ChainEvent>,
}

struct FakeChainState {
    height: u32,
    /// The transactions that the service sent, in the order of sending.
    sent: Vec<Transaction>,
    confirmed: HashMap<Txid, ConfirmedTx>,
    watched_outputs: Vec<OutPoint>,
    /// A problem with the peers that the chain reports. While it lasts, a broadcast fails.
    peer_problem: Option<PeerProblem>,
    /// Whether a broadcast fails, as it does when the connection is lost before the chain
    /// reports a problem.
    fail_broadcasts: bool,
    failed_broadcasts: usize,
    /// Whether no peer asks for an announced transaction.
    ignore_broadcasts: bool,
    ignored_broadcasts: usize,
}

impl FakeChain {
    const START_HEIGHT: u32 = 100;

    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(FakeChainState {
                height: Self::START_HEIGHT,
                sent: Vec::new(),
                confirmed: HashMap::new(),
                watched_outputs: Vec::new(),
                peer_problem: None,
                fail_broadcasts: false,
                failed_broadcasts: 0,
                ignore_broadcasts: false,
                ignored_broadcasts: 0,
            }),
            events: broadcast::channel(64).0,
        })
    }

    fn sent(&self) -> Vec<Transaction> {
        self.state.lock().unwrap().sent.clone()
    }

    fn watched_outputs(&self) -> Vec<OutPoint> {
        self.state.lock().unwrap().watched_outputs.clone()
    }

    fn fail_broadcasts(&self, fail: bool) {
        self.state.lock().unwrap().fail_broadcasts = fail;
    }

    fn failed_broadcasts(&self) -> usize {
        self.state.lock().unwrap().failed_broadcasts
    }

    fn ignore_broadcasts(&self, ignore: bool) {
        self.state.lock().unwrap().ignore_broadcasts = ignore;
    }

    fn ignored_broadcasts(&self) -> usize {
        self.state.lock().unwrap().ignored_broadcasts
    }

    /// Starts or ends a problem with the peers.
    fn set_peer_problem(&self, problem: Option<PeerProblem>) {
        self.state.lock().unwrap().peer_problem = problem;
        // The service looks at the chain again after every event.
        let height = self.state.lock().unwrap().height;
        let _ = self.events.send(ChainEvent::Synced { height });
    }

    /// Adds a block with the transactions `txs` to the chain.
    fn mine(&self, txs: &[Transaction]) {
        let height = {
            let mut state = self.state.lock().unwrap();
            state.height += 1;
            let height = state.height;
            for tx in txs {
                let confirmed = ConfirmedTx {
                    tx: Arc::new(tx.clone()),
                    height,
                    block_hash: BlockHash::all_zeros(),
                    block_time: 0,
                };
                state.confirmed.insert(tx.compute_txid(), confirmed);
            }
            height
        };
        for tx in txs {
            let _ = self.events.send(ChainEvent::Confirmed {
                txid: tx.compute_txid(),
                height,
            });
        }
        let _ = self.events.send(ChainEvent::Synced { height });
    }
}

impl ChainAccess for FakeChain {
    fn watch_script(&self, _script: ScriptBuf) {}

    fn watch_output(&self, outpoint: OutPoint, _script: ScriptBuf) {
        let mut state = self.state.lock().unwrap();
        if !state.watched_outputs.contains(&outpoint) {
            state.watched_outputs.push(outpoint);
        }
    }

    fn watch_outputs_from(&self, outputs: Vec<(OutPoint, ScriptBuf)>, _start: (u32, BlockHash)) {
        for (outpoint, script) in outputs {
            self.watch_output(outpoint, script);
        }
    }

    fn tip_height(&self) -> Option<u32> {
        Some(self.state.lock().unwrap().height)
    }

    fn synced_height(&self) -> Option<u32> {
        self.tip_height()
    }

    fn transaction(&self, txid: &Txid) -> Option<ConfirmedTx> {
        self.state.lock().unwrap().confirmed.get(txid).cloned()
    }

    fn confirmations(&self, txid: &Txid) -> u32 {
        let state = self.state.lock().unwrap();
        state
            .confirmed
            .get(txid)
            .map_or(0, |found| state.height - found.height + 1)
    }

    fn spending_tx(&self, outpoint: &OutPoint) -> Option<ConfirmedTx> {
        let state = self.state.lock().unwrap();
        state
            .confirmed
            .values()
            .find(|found| {
                found
                    .tx
                    .input
                    .iter()
                    .any(|input| input.previous_output == *outpoint)
            })
            .cloned()
    }

    fn peer_problem(&self) -> Option<PeerProblem> {
        self.state.lock().unwrap().peer_problem
    }

    fn subscribe(&self) -> broadcast::Receiver<ChainEvent> {
        self.events.subscribe()
    }

    fn broadcast(&self, tx: Transaction) -> BroadcastFuture {
        let txid = tx.compute_txid();
        let mut state = self.state.lock().unwrap();
        if let Some(problem) = state.peer_problem {
            return Box::pin(async move { Err(problem.into()) });
        }
        if state.fail_broadcasts {
            state.failed_broadcasts += 1;
            return Box::pin(async move { Err(ClientError::NotConnected) });
        }
        if state.ignore_broadcasts {
            state.ignored_broadcasts += 1;
            let waited = std::time::Duration::from_secs(10);
            return Box::pin(async move { Err(ClientError::NotRequested(waited)) });
        }
        state.sent.push(tx);
        Box::pin(async move { Ok(txid) })
    }
}

/// A running `musigrpc.Musig` service for one trader, with what the tests need to look into it
/// and to start it again.
struct RunningService {
    client: MusigClient<Channel>,
    trades: Arc<MemoryTradeStore>,
    /// The task of the server. When it is aborted, the service and its watcher end.
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

fn mock_wallet(is_buyer: bool) -> Arc<MockWalletProvider> {
    let wallet: Box<dyn ProtocolWalletApi + Send> = if is_buyer {
        Box::new(mock_buyer_trade_wallet())
    } else {
        Box::new(mock_seller_trade_wallet())
    };
    Arc::new(MockWalletProvider(tokio::sync::Mutex::new(wallet)))
}

/// Starts a `musigrpc.Musig` service for one trader on a free loopback port. Returns a connected
/// client, the trade store of the service, from which the tests read the state of a trade, and
/// the chain of the service.
async fn start_service(
    is_buyer: bool,
) -> (MusigClient<Channel>, Arc<MemoryTradeStore>, Arc<FakeChain>) {
    let chain = FakeChain::new();
    let (client, trades) = start_service_with_chain(is_buyer, Some(Arc::clone(&chain))).await;
    (client, trades, chain)
}

/// As [`start_service`], for a service with the given chain, or without access to a chain.
async fn start_service_with_chain(
    is_buyer: bool,
    chain: Option<Arc<FakeChain>>,
) -> (MusigClient<Channel>, Arc<MemoryTradeStore>) {
    let service = start_service_with_wallet(mock_wallet(is_buyer), chain).await;
    (service.client, service.trades)
}

/// Starts a service on a wallet. The trades whose records the wallet holds are loaded first, as
/// at the start of `musigd` when the wallet is opened.
async fn start_service_with_wallet(
    wallet: Arc<MockWalletProvider>,
    chain: Option<Arc<FakeChain>>,
) -> RunningService {
    let trades = Arc::new(MemoryTradeStore::default());
    restore_trades(&mut **wallet.0.lock().await, &*trades);
    let mut service = MusigImpl::new(
        Arc::clone(&trades) as _,
        wallet,
        EngineConfig::new(WARNING_LOCK_TIME, CLAIM_LOCK_TIME, 144, 6).expect("valid configuration"),
    );
    if let Some(chain) = chain {
        service = service.with_chain(chain as _);
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("free loopback port");
    let addr = listener.local_addr().expect("listener has a local address");
    let server = tokio::spawn(
        Server::builder()
            .add_service(MusigServer::new(service))
            .serve_with_incoming(TcpIncoming::from(listener)),
    );
    let client = MusigClient::connect(format!("http://{addr}"))
        .await
        .expect("service accepts connections");
    RunningService {
        client,
        trades,
        server,
    }
}

/// Returns a number that no other test of this process uses, to build unique trade IDs.
fn next_trade_number() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn mock_trade_fee_receiver() -> ReceiverAddressAndAmount {
    let (address, amount) = TRADE_FEE_RECEIVER;
    ReceiverAddressAndAmount {
        address: address.to_owned(),
        amount,
    }
}

fn mock_redirection_receivers() -> Vec<ReceiverAddressAndAmount> {
    REDIRECTION_RECEIVERS
        .iter()
        .map(|&(address, amount)| ReceiverAddressAndAmount {
            address: address.to_owned(),
            amount,
        })
        .collect()
}

/// One trader: a client of its own service, and the messages that this service returned so far.
struct Party {
    client: MusigClient<Channel>,
    trades: Arc<MemoryTradeStore>,
    chain: Arc<FakeChain>,
    /// The wallet of the trader. It holds the records of his trades and outlives a restart of
    /// the service.
    wallet: Arc<MockWalletProvider>,
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    trade_id: String,
    role: Role,
    key_shares: Option<PubKeySharesResponse>,
    nonce_shares: Option<NonceSharesMessage>,
    partial_signatures: Option<PartialSignaturesMessage>,
    deposit_psbt: Option<DepositPsbt>,
    custom_payout_psbt: Option<CustomPayoutPsbt>,
    close_response: Option<CloseTradeResponse>,
}

impl Party {
    async fn new(trade_id: String, role: Role) -> Self {
        let is_buyer = matches!(role, Role::BuyerAsMaker | Role::BuyerAsTaker);
        let chain = FakeChain::new();
        let wallet = mock_wallet(is_buyer);
        let service =
            start_service_with_wallet(Arc::clone(&wallet), Some(Arc::clone(&chain))).await;
        Self {
            client: service.client,
            trades: service.trades,
            chain,
            wallet,
            server: service.server,
            trade_id,
            role,
            key_shares: None,
            nonce_shares: None,
            partial_signatures: None,
            deposit_psbt: None,
            custom_payout_psbt: None,
            close_response: None,
        }
    }

    const fn is_buyer(&self) -> bool {
        matches!(self.role, Role::BuyerAsMaker | Role::BuyerAsTaker)
    }

    /// The state of this party's trade, read from the trade store of the service.
    /// Stops the service of this trader. Its wallet and its chain stay.
    async fn stop(&mut self) {
        self.server.abort();
        // The service and its watcher are gone when the task has ended.
        let _ = (&mut self.server).await;
    }

    /// Starts a new service for this trader, as after a start of `musigd`: with an empty trade
    /// store, which is loaded from the records in the wallet.
    async fn start(&mut self) {
        let service =
            start_service_with_wallet(Arc::clone(&self.wallet), Some(Arc::clone(&self.chain)))
                .await;
        self.client = service.client;
        self.trades = service.trades;
        self.server = service.server;
    }

    async fn restart(&mut self) {
        self.stop().await;
        self.start().await;
    }

    fn state(&self) -> TradeState {
        let trade_model = self
            .trades
            .get_trade_model(&self.trade_id)
            .expect("trade is stored");
        let state = trade_model.lock().unwrap().state();
        state
    }

    /// Whether the service terminated this party's trade.
    fn is_terminated(&self) -> bool {
        let trade_model = self
            .trades
            .get_trade_model(&self.trade_id)
            .expect("trade is stored");
        let is_terminated = trade_model.lock().unwrap().termination_reason().is_some();
        is_terminated
    }

    async fn init_trade(&mut self) -> Result<PubKeySharesResponse, Status> {
        let request = PubKeySharesRequest {
            trade_id: self.trade_id.clone(),
            my_role: self.role.into(),
        };
        self.client
            .init_trade(request)
            .await
            .map(Response::into_inner)
    }

    fn nonce_shares_request(&self, peers_key_shares: &PubKeySharesResponse) -> NonceSharesRequest {
        NonceSharesRequest {
            trade_id: self.trade_id.clone(),
            buyer_output_peers_pub_key_share: peers_key_shares.buyer_output_pub_key_share.clone(),
            seller_output_peers_pub_key_share: peers_key_shares.seller_output_pub_key_share.clone(),
            peers_multisig_script_key: peers_key_shares.multisig_script_key.clone(),
            deposit_tx_fee_rate: DEPOSIT_TX_FEE_RATE,
            prepared_tx_fee_rate: PREPARED_TX_FEE_RATE,
            trade_amount: TRADE_AMOUNT,
            buyers_security_deposit: BUYERS_SECURITY_DEPOSIT,
            sellers_security_deposit: SELLERS_SECURITY_DEPOSIT,
            trade_fee_receiver: Some(mock_trade_fee_receiver()),
        }
    }

    async fn get_nonce_shares(
        &mut self,
        peers_key_shares: &PubKeySharesResponse,
    ) -> Result<NonceSharesMessage, Status> {
        let request = self.nonce_shares_request(peers_key_shares);
        self.client
            .get_nonce_shares(request)
            .await
            .map(Response::into_inner)
    }

    async fn get_partial_signatures(
        &mut self,
        peers_nonce_shares: &NonceSharesMessage,
    ) -> Result<PartialSignaturesMessage, Status> {
        self.get_partial_signatures_with_receivers(peers_nonce_shares, mock_redirection_receivers())
            .await
    }

    async fn get_partial_signatures_with_receivers(
        &mut self,
        peers_nonce_shares: &NonceSharesMessage,
        redirection_receivers: Vec<ReceiverAddressAndAmount>,
    ) -> Result<PartialSignaturesMessage, Status> {
        let request = PartialSignaturesRequest {
            trade_id: self.trade_id.clone(),
            peers_nonce_shares: Some(peers_nonce_shares.clone()),
            redirection_receivers,
            buyer_ready_to_release: false,
        };
        self.client
            .get_partial_signatures(request)
            .await
            .map(Response::into_inner)
    }

    /// The second call of `GetPartialSignatures`, by which the buyer reports the started payment.
    async fn start_payment(&mut self) -> Result<PartialSignaturesMessage, Status> {
        let request = PartialSignaturesRequest {
            trade_id: self.trade_id.clone(),
            buyer_ready_to_release: true,
            ..PartialSignaturesRequest::default()
        };
        self.client
            .get_partial_signatures(request)
            .await
            .map(Response::into_inner)
    }

    async fn sign_deposit_tx(
        &mut self,
        peers_partial_signatures: &PartialSignaturesMessage,
    ) -> Result<DepositPsbt, Status> {
        let request = DepositTxSignatureRequest {
            trade_id: self.trade_id.clone(),
            // The Java client removes the contractual transaction IDs before it passes the message
            // on, because the service ignores them.
            peers_partial_signatures: Some(PartialSignaturesMessage {
                contractual_tx_ids: None,
                ..peers_partial_signatures.clone()
            }),
        };
        self.client
            .sign_deposit_tx(request)
            .await
            .map(Response::into_inner)
    }

    async fn publish_deposit_tx(
        &mut self,
        peers_deposit_psbt: &DepositPsbt,
    ) -> Result<Streaming<TxConfirmationStatus>, Status> {
        let request = PublishDepositTxRequest {
            trade_id: self.trade_id.clone(),
            peers_deposit_psbt: Some(peers_deposit_psbt.clone()),
        };
        self.client
            .publish_deposit_tx(request)
            .await
            .map(Response::into_inner)
    }

    async fn subscribe_tx_confirmation_status(
        &mut self,
    ) -> Result<Streaming<TxConfirmationStatus>, Status> {
        let request = SubscribeTxConfirmationStatusRequest {
            trade_id: self.trade_id.clone(),
        };
        self.client
            .subscribe_tx_confirmation_status(request)
            .await
            .map(Response::into_inner)
    }

    /// The first call of `SignSwapTx`, by which the seller stores the buyer's partial signature.
    async fn sign_swap_tx(
        &mut self,
        peers_partial_signature: &[u8],
    ) -> Result<SwapTxSignatureResponse, Status> {
        let request = SwapTxSignatureRequest {
            trade_id: self.trade_id.clone(),
            swap_tx_input_peers_partial_signature: peers_partial_signature.to_vec(),
            seller_ready_to_release: false,
        };
        self.client
            .sign_swap_tx(request)
            .await
            .map(Response::into_inner)
    }

    /// The second call of `SignSwapTx`, by which the seller confirms the payment.
    async fn release_payout(&mut self) -> Result<SwapTxSignatureResponse, Status> {
        let request = SwapTxSignatureRequest {
            trade_id: self.trade_id.clone(),
            seller_ready_to_release: true,
            ..SwapTxSignatureRequest::default()
        };
        self.client
            .sign_swap_tx(request)
            .await
            .map(Response::into_inner)
    }

    /// Calls `CloseTrade`. The trade ID of `request` is replaced by the ID of this party's trade.
    async fn close_trade(
        &mut self,
        request: CloseTradeRequest,
    ) -> Result<CloseTradeResponse, Status> {
        let request = CloseTradeRequest {
            trade_id: self.trade_id.clone(),
            ..request
        };
        self.client
            .close_trade(request)
            .await
            .map(Response::into_inner)
    }

    async fn close_trade_with_key_share(
        &mut self,
        peers_prv_key_share: &[u8],
    ) -> Result<CloseTradeResponse, Status> {
        self.close_trade(CloseTradeRequest {
            my_output_peers_prv_key_share: Some(peers_prv_key_share.to_vec()),
            // The Java client sets a penalty fee rate only in the call of the buyer.
            penalty_tx_fee_rate: self.is_buyer().then_some(BUYERS_PENALTY_TX_FEE_RATE),
            ..CloseTradeRequest::default()
        })
        .await
    }

    async fn close_trade_with_swap_tx(
        &mut self,
        swap_tx: &[u8],
    ) -> Result<CloseTradeResponse, Status> {
        self.close_trade(CloseTradeRequest {
            swap_tx: Some(swap_tx.to_vec()),
            ..CloseTradeRequest::default()
        })
        .await
    }

    async fn close_trade_without_arguments(&mut self) -> Result<CloseTradeResponse, Status> {
        self.close_trade(CloseTradeRequest::default()).await
    }

    async fn publish_warning_tx(&mut self) -> Result<PublishWarningTxResponse, Status> {
        let request = PublishWarningTxRequest {
            trade_id: self.trade_id.clone(),
        };
        self.client
            .publish_warning_tx(request)
            .await
            .map(Response::into_inner)
    }

    async fn subscribe_trade_events(&mut self) -> Result<Streaming<TradeEvent>, Status> {
        let request = SubscribeTradeEventsRequest {
            trade_id: self.trade_id.clone(),
        };
        self.client
            .subscribe_trade_events(request)
            .await
            .map(Response::into_inner)
    }

    async fn sign_custom_payout_tx(
        &mut self,
        sellers_payout_amount_excluding_fee: u64,
    ) -> Result<CustomPayoutPsbt, Status> {
        let request = CustomPayoutPsbtRequest {
            trade_id: self.trade_id.clone(),
            sellers_payout_amount_excluding_fee,
            fee_rate: CUSTOM_PAYOUT_TX_FEE_RATE,
        };
        self.client
            .sign_custom_payout_tx(request)
            .await
            .map(Response::into_inner)
    }

    async fn abandon_trade(&mut self) -> Result<AbandonTradeResponse, Status> {
        let request = AbandonTradeRequest {
            trade_id: self.trade_id.clone(),
        };
        self.client
            .abandon_trade(request)
            .await
            .map(Response::into_inner)
    }

    async fn custom_close_trade(
        &mut self,
        peers_custom_payout_psbt: &[u8],
    ) -> Result<CustomCloseTradeResponse, Status> {
        let request = CustomCloseTradeRequest {
            trade_id: self.trade_id.clone(),
            peers_custom_payout_psbt: peers_custom_payout_psbt.to_vec(),
        };
        self.client
            .custom_close_trade(request)
            .await
            .map(Response::into_inner)
    }
}

/// The amount that the seller receives in the custom payout of the Java test client, before the
/// seller's half of the fee is subtracted.
const SELLERS_CUSTOM_PAYOUT_AMOUNT: u64 = 245_000;

/// The two sides of one trade, and the carrier of the messages between them.
///
/// Each step method performs the service calls of one protocol step in the order of the Java
/// client, passes the returned messages to the other side, and checks the states of both sides.
struct Trade {
    kind: TradeType,
    buyer: Party,
    seller: Party,
    /// Message E: the buyer's partial signatures, now with the signature for the swap transaction.
    payment_started: Option<PartialSignaturesMessage>,
    /// Message F: the seller's private key share for the buyer's payout.
    release: Option<SwapTxSignatureResponse>,
}

impl Trade {
    async fn new(trade_type: TradeType) -> Self {
        let number = next_trade_number();
        let (buyer_role, seller_role) = match trade_type {
            TradeType::TakerIsBuyer => (Role::BuyerAsTaker, Role::SellerAsMaker),
            TradeType::TakerIsSeller => (Role::BuyerAsMaker, Role::SellerAsTaker),
        };
        Self {
            kind: trade_type,
            buyer: Party::new(format!("trade-{number}"), buyer_role).await,
            seller: Party::new(format!("trade-{number}"), seller_role).await,
            payment_started: None,
            release: None,
        }
    }

    /// Creates a trade and runs it up to the given stage.
    async fn at(trade_type: TradeType, stage: Stage) -> Self {
        let mut trade = Self::new(trade_type).await;
        trade.init_trades().await;
        trade.exchange_nonce_shares().await;
        trade.exchange_partial_signatures().await;
        if stage == Stage::SignaturesExchanged {
            return trade;
        }
        trade.sign_and_publish_deposit_tx().await;
        match stage {
            Stage::DepositSigned => return trade,
            Stage::CustomPayoutSigned => {
                trade.sign_custom_payout_tx().await;
                return trade;
            }
            Stage::ClosedByCustomPayout => {
                trade.sign_custom_payout_tx().await;
                trade.close_by_custom_payout().await;
                return trade;
            }
            _ => {}
        }
        trade.buyer_starts_payment().await;
        if stage == Stage::BuyerStartedPayment {
            return trade;
        }
        trade.seller_receives_payment_start().await;
        match stage {
            Stage::PaymentStarted => return trade,
            Stage::CustomPayoutSignedAfterPaymentStart => {
                trade.sign_custom_payout_tx().await;
                return trade;
            }
            _ => {}
        }
        trade.release_payout().await;
        match stage {
            Stage::SellerReleased => {}
            Stage::BuyerClosedCooperatively => trade.buyer_closes_cooperatively().await,
            Stage::ClosedCooperatively => {
                trade.buyer_closes_cooperatively().await;
                trade.seller_closes_cooperatively().await;
            }
            Stage::SellerClosedByForce => trade.seller_closes_by_force().await,
            Stage::ClosedByForce => {
                trade.seller_closes_by_force().await;
                trade.buyer_closes_with_swap_tx().await;
            }
            _ => unreachable!("all other stages returned above"),
        }
        trade
    }

    const fn party(&self, side: Side) -> &Party {
        match side {
            Side::Buyer => &self.buyer,
            Side::Seller => &self.seller,
        }
    }

    const fn party_mut(&mut self, side: Side) -> &mut Party {
        match side {
            Side::Buyer => &mut self.buyer,
            Side::Seller => &mut self.seller,
        }
    }

    const fn taker_side(&self) -> Side {
        match self.kind {
            TradeType::TakerIsBuyer => Side::Buyer,
            TradeType::TakerIsSeller => Side::Seller,
        }
    }

    const fn maker_side(&self) -> Side {
        match self.kind {
            TradeType::TakerIsBuyer => Side::Seller,
            TradeType::TakerIsSeller => Side::Buyer,
        }
    }

    #[track_caller]
    fn assert_states(&self, buyer_state: TradeState, seller_state: TradeState) {
        assert_eq!(
            self.buyer.state(),
            buyer_state,
            "state of the buyer ({:?})",
            self.kind
        );
        assert_eq!(
            self.seller.state(),
            seller_state,
            "state of the seller ({:?})",
            self.kind
        );
    }

    /// Both sides call `InitTrade`, the taker first. The taker's response is message A.
    async fn init_trades(&mut self) {
        for side in [self.taker_side(), self.maker_side()] {
            let party = self.party_mut(side);
            let key_shares = party.init_trade().await.expect("InitTrade succeeds");
            party.key_shares = Some(key_shares);
        }
        self.assert_states(TradeState::Init, TradeState::Init);
    }

    /// Both sides call `GetNonceShares`, the maker first. The maker's response is message B.
    async fn exchange_nonce_shares(&mut self) {
        for (side, peer_side) in [
            (self.maker_side(), self.taker_side()),
            (self.taker_side(), self.maker_side()),
        ] {
            let peers_key_shares = self
                .party(peer_side)
                .key_shares
                .clone()
                .expect("message A/B");
            let party = self.party_mut(side);
            let nonce_shares = party
                .get_nonce_shares(&peers_key_shares)
                .await
                .expect("GetNonceShares succeeds");
            party.nonce_shares = Some(nonce_shares);
        }
        self.assert_states(TradeState::Init, TradeState::Init);
    }

    /// Both sides call `GetPartialSignatures`, the taker first. The taker's response is message C.
    async fn exchange_partial_signatures(&mut self) {
        for (side, peer_side) in [
            (self.taker_side(), self.maker_side()),
            (self.maker_side(), self.taker_side()),
        ] {
            let peers_nonce_shares = self
                .party(peer_side)
                .nonce_shares
                .clone()
                .expect("message B/C");
            let party = self.party_mut(side);
            let partial_signatures = party
                .get_partial_signatures(&peers_nonce_shares)
                .await
                .expect("GetPartialSignatures succeeds");
            party.partial_signatures = Some(partial_signatures);
        }
        self.assert_states(TradeState::Init, TradeState::Init);
    }

    /// Both sides call `SignDepositTx`, the maker first. The maker's responses are message D.
    async fn sign_deposit_tx(&mut self) {
        for (side, peer_side) in [
            (self.maker_side(), self.taker_side()),
            (self.taker_side(), self.maker_side()),
        ] {
            let peers_partial_signatures = self
                .party(peer_side)
                .partial_signatures
                .clone()
                .expect("message C/D");
            let party = self.party_mut(side);
            let deposit_psbt = party
                .sign_deposit_tx(&peers_partial_signatures)
                .await
                .expect("SignDepositTx succeeds");
            party.deposit_psbt = Some(deposit_psbt);
            assert_eq!(party.state(), TradeState::Deposit);
        }
    }

    /// The taker calls `PublishDepositTx` with the maker's signed PSBT.
    async fn publish_deposit_tx(&mut self) -> Streaming<TxConfirmationStatus> {
        let makers_deposit_psbt = self
            .party(self.maker_side())
            .deposit_psbt
            .clone()
            .expect("message D");
        let taker = self.party_mut(self.taker_side());
        let confirmations = taker
            .publish_deposit_tx(&makers_deposit_psbt)
            .await
            .expect("PublishDepositTx succeeds");
        self.assert_states(TradeState::Deposit, TradeState::Deposit);
        confirmations
    }

    /// Signs and publishes the deposit transaction, and confirms it on the chains of both
    /// services, so that it counts as confirmed (D6).
    async fn sign_and_publish_deposit_tx(&mut self) {
        self.sign_deposit_tx().await;
        drop(self.publish_deposit_tx().await);
        self.confirm_deposit().await;
    }

    /// Mines the deposit transaction that the taker's service sent, and two more blocks, on the
    /// chains of both services.
    async fn confirm_deposit(&mut self) {
        let takers_chain = Arc::clone(&self.party(self.taker_side()).chain);
        assert!(
            sent_count_reaches(&takers_chain, 1).await,
            "the taker's service sends the deposit transaction"
        );
        let deposit_tx = takers_chain.sent().remove(0);
        self.mine(std::slice::from_ref(&deposit_tx));
        self.mine(&[]);
        self.mine(&[]);
    }

    /// Adds a block with the transactions `txs` to the chains of both services.
    fn mine(&self, txs: &[Transaction]) {
        for side in SIDES {
            self.party(side).chain.mine(txs);
        }
    }

    /// The buyer reports the started payment. The response is message E.
    async fn buyer_starts_payment(&mut self) {
        let payment_started = self
            .buyer
            .start_payment()
            .await
            .expect("buyer starts the payment");
        self.payment_started = Some(payment_started);
        self.assert_states(TradeState::BuyerReadyToRelease, TradeState::Deposit);
    }

    /// The buyer's partial signature for the swap transaction, taken from message E.
    fn buyers_swap_tx_signature(&self) -> Vec<u8> {
        self.payment_started
            .as_ref()
            .and_then(|message| message.swap_tx_input_partial_signature.clone())
            .expect("message E contains the buyer's partial signature for the swap transaction")
    }

    /// The seller stores the buyer's partial signature for the swap transaction.
    async fn seller_receives_payment_start(&mut self) {
        let signature = self.buyers_swap_tx_signature();
        let response = self
            .seller
            .sign_swap_tx(&signature)
            .await
            .expect("seller accepts message E");
        assert_eq!(
            response,
            SwapTxSignatureResponse::default(),
            "no key share before release"
        );
        self.assert_states(
            TradeState::BuyerReadyToRelease,
            TradeState::BuyerReadyToRelease,
        );
    }

    /// The seller confirms the payment. The response is message F.
    async fn release_payout(&mut self) {
        let release = self
            .seller
            .release_payout()
            .await
            .expect("seller releases the payout");
        self.release = Some(release);
        self.assert_states(
            TradeState::BuyerReadyToRelease,
            TradeState::SellerReadyToRelease,
        );
    }

    /// The seller's private key share for the buyer's payout, taken from message F.
    fn sellers_key_share(&self) -> Vec<u8> {
        self.release
            .as_ref()
            .and_then(|release| release.peer_output_prv_key_share.clone())
            .expect("message F contains the seller's key share")
    }

    /// The buyer closes with message F. The response contains message G.
    async fn buyer_closes_cooperatively(&mut self) {
        let sellers_key_share = self.sellers_key_share();
        let response = self
            .buyer
            .close_trade_with_key_share(&sellers_key_share)
            .await
            .expect("buyer closes the trade");
        self.buyer.close_response = Some(response);
        self.assert_states(
            TradeState::TradeClosed(ClosureType::Cooperative),
            TradeState::SellerReadyToRelease,
        );
    }

    /// The buyer's private key share for the seller's payout, taken from message G.
    fn buyers_key_share(&self) -> Vec<u8> {
        self.buyer
            .close_response
            .as_ref()
            .and_then(|response| response.peer_output_prv_key_share.clone())
            .expect("message G contains the buyer's key share")
    }

    /// The seller closes with message G.
    async fn seller_closes_cooperatively(&mut self) {
        let buyers_key_share = self.buyers_key_share();
        let response = self
            .seller
            .close_trade_with_key_share(&buyers_key_share)
            .await
            .expect("seller closes the trade");
        self.seller.close_response = Some(response);
        let closed = TradeState::TradeClosed(ClosureType::Cooperative);
        self.assert_states(closed, closed);
    }

    /// The seller closes without message G. The response contains the swap transaction.
    async fn seller_closes_by_force(&mut self) {
        let buyers_state = self.buyer.state();
        let response = self
            .seller
            .close_trade_without_arguments()
            .await
            .expect("seller closes the trade with the swap transaction");
        self.seller.close_response = Some(response);
        self.assert_states(buyers_state, TradeState::TradeClosed(ClosureType::Forced));
    }

    /// The swap transaction that the seller's service returned.
    fn swap_tx(&self) -> Vec<u8> {
        self.seller
            .close_response
            .as_ref()
            .and_then(|response| response.swap_tx.clone())
            .expect("seller's service returned the swap transaction")
    }

    /// The buyer closes with the swap transaction, which stands for the transaction that the buyer
    /// finds on chain.
    async fn buyer_closes_with_swap_tx(&mut self) {
        let swap_tx = self.swap_tx();
        let response = self
            .buyer
            .close_trade_with_swap_tx(&swap_tx)
            .await
            .expect("buyer closes the trade with the swap transaction");
        self.buyer.close_response = Some(response);
        let closed = TradeState::TradeClosed(ClosureType::Forced);
        self.assert_states(closed, closed);
    }

    /// Both sides call `SignCustomPayoutTx` with the same amount, the buyer first.
    async fn sign_custom_payout_tx(&mut self) {
        for side in SIDES {
            let party = self.party_mut(side);
            let custom_payout_psbt = party
                .sign_custom_payout_tx(SELLERS_CUSTOM_PAYOUT_AMOUNT)
                .await
                .expect("SignCustomPayoutTx succeeds");
            party.custom_payout_psbt = Some(custom_payout_psbt);
        }
        self.assert_states(
            TradeState::CustomPayoutSigned,
            TradeState::CustomPayoutSigned,
        );
    }

    /// Both sides call `CustomCloseTrade` with the PSBT of the peer, the seller first.
    async fn close_by_custom_payout(&mut self) -> [CustomCloseTradeResponse; 2] {
        let buyers_psbt = self
            .buyer
            .custom_payout_psbt
            .clone()
            .expect("buyer signed")
            .psbt;
        let sellers_psbt = self
            .seller
            .custom_payout_psbt
            .clone()
            .expect("seller signed")
            .psbt;
        let sellers_response = self
            .seller
            .custom_close_trade(&buyers_psbt)
            .await
            .expect("seller closes the trade");
        let buyers_response = self
            .buyer
            .custom_close_trade(&sellers_psbt)
            .await
            .expect("buyer closes the trade");

        let closed = TradeState::TradeClosed(ClosureType::Custom);
        self.assert_states(closed, closed);
        [buyers_response, sellers_response]
    }
}

fn tx(bytes: &[u8]) -> Transaction {
    consensus::deserialize(bytes).expect("valid transaction")
}

fn psbt(bytes: &[u8]) -> Psbt {
    Psbt::deserialize(bytes).expect("valid PSBT")
}

fn address(address: &str) -> Address {
    address
        .parse::<Address<_>>()
        .expect("valid address")
        .require_network(Network::Regtest)
        .expect("regtest address")
}

fn txid(txid: &str) -> Txid {
    txid.parse().expect("valid transaction ID")
}

/// A valid encoding of a private key share or of a partial signature that belongs to no trade.
fn random_scalar() -> Vec<u8> {
    Scalar::random(&mut rand::rng()).serialize().into()
}

/// Checks that input `input_index` of `tx` is a valid Taproot key spend of `prevouts[input_index]`.
#[track_caller]
fn assert_valid_key_spend(tx: &Transaction, input_index: usize, prevouts: &[TxOut]) {
    let witness = &tx.input[input_index].witness;
    assert_eq!(witness.len(), 1, "a key spend has one witness element");
    let signature = taproot::Signature::from_slice(&witness[0]).expect("valid signature encoding");
    assert_eq!(signature.sighash_type, TapSighashType::Default);

    let script_pubkey = &prevouts[input_index].script_pubkey;
    assert!(
        script_pubkey.is_p2tr(),
        "the spent output is a Taproot output"
    );
    let output_key =
        XOnlyPublicKey::from_slice(&script_pubkey.as_bytes()[2..]).expect("valid output key");
    let sighash = SighashCache::new(tx)
        .taproot_key_spend_signature_hash(
            input_index,
            &Prevouts::All(prevouts),
            TapSighashType::Default,
        )
        .expect("sighash can be computed");
    Secp256k1::verification_only()
        .verify_schnorr(
            &signature.signature,
            &Message::from_digest(sighash.to_byte_array()),
            &output_key,
        )
        .expect("the signature is valid for the spent output");
}

/// Returns the outpoint and the output of the only output of `tx` with the given amount.
#[track_caller]
fn output_with_amount(tx: &Transaction, amount: u64) -> (OutPoint, TxOut) {
    let mut matching = tx
        .output
        .iter()
        .enumerate()
        .filter(|(_, o)| o.value.to_sat() == amount);
    let (vout, output) = matching.next().expect("an output with the amount exists");
    assert!(matching.next().is_none(), "only one output has the amount");
    (
        OutPoint::new(tx.compute_txid(), u32::try_from(vout).expect("small index")),
        output.clone(),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Call {
    GetNonceShares,
    GetPartialSignatures,
    StartPayment,
    SignDepositTx,
    PublishDepositTx,
    SubscribeTxConfirmationStatus,
    SignSwapTx,
    ReleasePayout,
    CloseTradeWithKeyShare,
    CloseTradeWithSwapTx,
    CloseTradeWithoutArguments,
    SignCustomPayoutTx,
    CustomCloseTrade,
}

const CALLS: [Call; 13] = [
    Call::GetNonceShares,
    Call::GetPartialSignatures,
    Call::StartPayment,
    Call::SignDepositTx,
    Call::PublishDepositTx,
    Call::SubscribeTxConfirmationStatus,
    Call::SignSwapTx,
    Call::ReleasePayout,
    Call::CloseTradeWithKeyShare,
    Call::CloseTradeWithSwapTx,
    Call::CloseTradeWithoutArguments,
    Call::SignCustomPayoutTx,
    Call::CustomCloseTrade,
];

/// A transaction with a valid encoding that spends no output of any trade.
fn unrelated_tx() -> Vec<u8> {
    consensus::serialize(&Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn::default()],
        output: vec![TxOut {
            value: Amount::from_sat(1_000),
            script_pubkey: ScriptBuf::new(),
        }],
    })
}

/// A PSBT with a valid encoding that belongs to no trade.
fn unrelated_psbt() -> Vec<u8> {
    let unsigned_tx = Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn::default()],
        output: vec![],
    };
    Psbt::from_unsigned_tx(unsigned_tx)
        .expect("transaction is unsigned")
        .serialize()
}

impl Trade {
    /// The transaction IDs that both services returned with the partial signatures.
    fn contractual_tx_ids(&self) -> ContractualTxIds {
        let ids = |party: &Party| {
            party
                .partial_signatures
                .as_ref()
                .and_then(|message| message.contractual_tx_ids.clone())
        };
        let buyers_ids = ids(&self.buyer).expect("buyer's service returned the transaction IDs");
        let sellers_ids = ids(&self.seller).expect("seller's service returned the transaction IDs");
        assert_eq!(
            buyers_ids, sellers_ids,
            "both sides computed the same transactions"
        );
        buyers_ids
    }

    /// The unsigned deposit transaction, taken from the deposit PSBTs of both sides.
    fn deposit_tx(&self) -> Transaction {
        let unsigned_tx = |party: &Party| {
            psbt(
                &party
                    .deposit_psbt
                    .as_ref()
                    .expect("deposit is signed")
                    .deposit_psbt,
            )
            .unsigned_tx
        };
        let deposit_tx = unsigned_tx(&self.buyer);
        assert_eq!(
            deposit_tx,
            unsigned_tx(&self.seller),
            "both sides signed the same transaction"
        );
        deposit_tx
    }

    /// The output of the deposit transaction that holds the trade amount and the buyer's deposit.
    fn buyer_payout(&self) -> (OutPoint, TxOut) {
        output_with_amount(&self.deposit_tx(), TRADE_AMOUNT + BUYERS_SECURITY_DEPOSIT)
    }

    /// The output of the deposit transaction that holds the seller's deposit.
    fn seller_payout(&self) -> (OutPoint, TxOut) {
        output_with_amount(&self.deposit_tx(), SELLERS_SECURITY_DEPOSIT)
    }

    fn claim_tx_payout_address(&self, side: Side) -> Address {
        let nonce_shares = self.party(side).nonce_shares.as_ref().expect("message B/C");
        address(&nonce_shares.claim_tx_payout_address)
    }

    /// Lets one side perform `call`. The arguments are the messages that the peer sent in this
    /// trade. If the peer did not yet send the message at the current stage, the argument is a
    /// value with a valid encoding that belongs to no trade.
    async fn call(&mut self, side: Side, call: Call) -> Result<(), Status> {
        let peer = self.party(if side == Side::Buyer {
            Side::Seller
        } else {
            Side::Buyer
        });
        let peers_key_shares = peer.key_shares.clone().expect("message A/B");
        let peers_nonce_shares = peer.nonce_shares.clone().expect("message B/C");
        let peers_partial_signatures = peer.partial_signatures.clone().expect("message C/D");
        let peers_deposit_psbt = peer.deposit_psbt.clone().unwrap_or_else(|| DepositPsbt {
            deposit_psbt: unrelated_psbt(),
        });
        let peers_custom_payout_psbt = peer
            .custom_payout_psbt
            .as_ref()
            .map_or_else(unrelated_psbt, |p| p.psbt.clone());
        let buyers_swap_tx_signature = self
            .payment_started
            .as_ref()
            .and_then(|message| message.swap_tx_input_partial_signature.clone())
            .unwrap_or_else(random_scalar);
        let peers_key_share = match side {
            Side::Buyer => self
                .release
                .as_ref()
                .and_then(|r| r.peer_output_prv_key_share.clone()),
            Side::Seller => self
                .buyer
                .close_response
                .as_ref()
                .and_then(|r| r.peer_output_prv_key_share.clone()),
        }
        .unwrap_or_else(random_scalar);
        let swap_tx = self
            .seller
            .close_response
            .as_ref()
            .and_then(|response| response.swap_tx.clone())
            .unwrap_or_else(unrelated_tx);

        let party = self.party_mut(side);
        match call {
            Call::GetNonceShares => party.get_nonce_shares(&peers_key_shares).await.map(drop),
            Call::GetPartialSignatures => party
                .get_partial_signatures(&peers_nonce_shares)
                .await
                .map(drop),
            Call::StartPayment => party.start_payment().await.map(drop),
            Call::SignDepositTx => party
                .sign_deposit_tx(&peers_partial_signatures)
                .await
                .map(drop),
            Call::PublishDepositTx => party
                .publish_deposit_tx(&peers_deposit_psbt)
                .await
                .map(drop),
            Call::SubscribeTxConfirmationStatus => {
                party.subscribe_tx_confirmation_status().await.map(drop)
            }
            Call::SignSwapTx => party
                .sign_swap_tx(&buyers_swap_tx_signature)
                .await
                .map(drop),
            Call::ReleasePayout => party.release_payout().await.map(drop),
            Call::CloseTradeWithKeyShare => party
                .close_trade_with_key_share(&peers_key_share)
                .await
                .map(drop),
            Call::CloseTradeWithSwapTx => party.close_trade_with_swap_tx(&swap_tx).await.map(drop),
            Call::CloseTradeWithoutArguments => {
                party.close_trade_without_arguments().await.map(drop)
            }
            Call::SignCustomPayoutTx => party
                .sign_custom_payout_tx(SELLERS_CUSTOM_PAYOUT_AMOUNT)
                .await
                .map(drop),
            Call::CustomCloseTrade => party
                .custom_close_trade(&peers_custom_payout_psbt)
                .await
                .map(drop),
        }
    }
}

/// Checks that `result` is an error with the given code and a message that contains `message`.
#[track_caller]
fn assert_refused<T>(result: Result<T, Status>, code: Code, message: &str) {
    let Err(status) = result else {
        panic!("the call must be refused ({message})")
    };
    assert_eq!(status.code(), code, "unexpected status: {status}");
    assert!(
        status.message().contains(message),
        "unexpected status: {status}"
    );
}

/// Checks that the service terminated the trade of `party` with this response: the status is
/// `ABORTED`, and its message names the termination and contains `reason`.
#[track_caller]
fn assert_terminated<T>(result: Result<T, Status>, party: &Party, reason: &str) {
    let Err(status) = result else {
        panic!("the trade must be terminated ({reason})")
    };
    assert_eq!(status.code(), Code::Aborted, "unexpected status: {status}");
    assert!(
        status.message().contains("trade was terminated") && status.message().contains(reason),
        "unexpected status: {status}"
    );
    assert!(party.is_terminated());
}

/// Checks that the request was refused because it is not allowed in the state `state`, and that
/// the trade was not terminated.
#[track_caller]
fn assert_not_allowed<T>(result: Result<T, Status>, party: &Party, state: TradeState) {
    let message = format!("request is not allowed in state {state}");
    assert_refused(result, Code::FailedPrecondition, &message);
    assert!(!party.is_terminated());
    assert_eq!(party.state(), state);
}

/// Checks that `key_share` is the private key of the public key share `pub_key_share`.
#[track_caller]
fn assert_key_pair(key_share: &[u8], pub_key_share: &[u8]) {
    let private_key = Scalar::try_from(key_share).expect("valid private key share");
    let public_key = Point::try_from(pub_key_share).expect("valid public key share");
    assert_eq!(private_key.base_point_mul(), public_key);
}

/// Checks the penalty transaction of one side: it spends the escrow output of the peer's warning
/// transaction to the claim address of this side. The signature cannot be checked here, because
/// the service does not return the warning transaction.
#[track_caller]
fn assert_penalty_tx(
    penalty_tx: Option<&Vec<u8>>,
    peers_warning_tx_id: &str,
    payout_address: &Address,
    fee_rate: u64,
) {
    let penalty_tx = tx(penalty_tx.expect("the response contains the penalty transaction"));
    assert_eq!(penalty_tx.input.len(), 1);
    assert_eq!(
        penalty_tx.input[0].previous_output,
        OutPoint::new(txid(peers_warning_tx_id), 0)
    );
    assert_eq!(
        penalty_tx.input[0].sequence,
        Sequence::ENABLE_RBF_NO_LOCKTIME
    );
    assert_eq!(penalty_tx.input[0].witness.len(), 1, "key spend");
    assert_eq!(penalty_tx.input[0].witness[0].len(), 64);
    assert_eq!(
        penalty_tx.output,
        [TxOut {
            value: Amount::from_sat(ESCROW_AMOUNT - fee(fee_rate, FORWARDING_TX_WEIGHT)),
            script_pubkey: payout_address.script_pubkey(),
        }]
    );
}

// ---- Setup and deposit ----

/// Setup of a trade up to the published deposit transaction, as far as the service allows without
/// a chain (parts of A1 and A9).
async fn check_setup_and_deposit(trade_type: TradeType) {
    let mut trade = Trade::new(trade_type).await;
    check_setup_messages(&mut trade).await;
    check_deposit(&mut trade).await;
}

/// Exchanges the messages A to C and checks their contents.
async fn check_setup_messages(trade: &mut Trade) {
    trade.init_trades().await;
    let mut pub_key_shares = Vec::new();
    for side in SIDES {
        let key_shares = trade.party(side).key_shares.as_ref().expect("message A/B");
        for key_share in [
            &key_shares.buyer_output_pub_key_share,
            &key_shares.seller_output_pub_key_share,
        ] {
            pub_key_shares.push(Point::try_from(&key_share[..]).expect("valid public key share"));
        }
        XOnlyPublicKey::from_slice(&key_shares.multisig_script_key).expect("valid script key");
    }
    pub_key_shares.sort();
    pub_key_shares.dedup();
    assert_eq!(pub_key_shares.len(), 4, "all key shares differ");

    trade.exchange_nonce_shares().await;
    for side in SIDES {
        let nonce_shares = trade
            .party(side)
            .nonce_shares
            .as_ref()
            .expect("message B/C");
        assert_eq!(
            nonce_shares.redirection_amount_msat,
            REDIRECTION_AMOUNT_MSAT
        );
        address(&nonce_shares.warning_tx_fee_bump_address);
        address(&nonce_shares.redirect_tx_fee_bump_address);
        address(&nonce_shares.claim_tx_payout_address);

        // The first output of a half deposit PSBT is a placeholder with the amount that this side
        // pays into the deposit. The seller's half also pays the trade fee.
        let outputs = psbt(&nonce_shares.half_deposit_psbt).unsigned_tx.output;
        assert!(outputs[0].script_pubkey.is_op_return());
        match side {
            Side::Buyer => assert_eq!(outputs[0].value.to_sat(), BUYERS_SECURITY_DEPOSIT),
            Side::Seller => {
                assert_eq!(
                    outputs[0].value.to_sat(),
                    TRADE_AMOUNT + SELLERS_SECURITY_DEPOSIT
                );
                assert_eq!(outputs[1].value.to_sat(), TRADE_FEE_RECEIVER.1);
                assert_eq!(
                    outputs[1].script_pubkey,
                    address(TRADE_FEE_RECEIVER.0).script_pubkey()
                );
            }
        }
    }

    trade.exchange_partial_signatures().await;
    let ids = trade.contractual_tx_ids();
    let mut distinct_ids = vec![
        txid(&ids.deposit_tx_id),
        txid(&ids.buyers_warning_tx_id),
        txid(&ids.sellers_warning_tx_id),
        txid(&ids.buyers_redirect_tx_id),
        txid(&ids.sellers_redirect_tx_id),
    ];
    distinct_ids.sort();
    distinct_ids.dedup();
    assert_eq!(distinct_ids.len(), 5, "all transactions differ");

    // The buyer withholds the partial signature for the swap transaction until the payment has
    // started. Only the seller can compute the sighash of the swap transaction.
    let buyers_signatures = trade
        .buyer
        .partial_signatures
        .as_ref()
        .expect("message C/D");
    assert_eq!(buyers_signatures.swap_tx_input_partial_signature, None);
    assert_eq!(buyers_signatures.swap_tx_input_sighash, None);
    let sellers_signatures = trade
        .seller
        .partial_signatures
        .as_ref()
        .expect("message C/D");
    assert!(sellers_signatures.swap_tx_input_partial_signature.is_some());
    assert_eq!(
        sellers_signatures
            .swap_tx_input_sighash
            .as_ref()
            .map(Vec::len),
        Some(32)
    );
}

/// Reads the next event of a confirmation stream, with a limit for the waiting time.
async fn next_event(events: &mut Streaming<TxConfirmationStatus>) -> Option<TxConfirmationStatus> {
    tokio::time::timeout(std::time::Duration::from_secs(10), events.next())
        .await
        .expect("the stream answers in time")
        .map(|event| event.expect("event without error"))
}

/// Signs and publishes the deposit transaction and checks it and the confirmation events (F3):
/// the publisher sends the transaction to the network, both traders watch the outputs of the
/// trade, and both get one event for each of the first three confirmations.
async fn check_deposit(trade: &mut Trade) {
    trade.sign_deposit_tx().await;
    let deposit_tx = trade.deposit_tx();
    let ids = trade.contractual_tx_ids();
    assert_eq!(deposit_tx.compute_txid(), txid(&ids.deposit_tx_id));
    assert!(trade.buyer_payout().1.script_pubkey.is_p2tr());
    assert!(trade.seller_payout().1.script_pubkey.is_p2tr());
    let (_, trade_fee_output) = output_with_amount(&deposit_tx, TRADE_FEE_RECEIVER.1);
    assert_eq!(
        trade_fee_output.script_pubkey,
        address(TRADE_FEE_RECEIVER.0).script_pubkey()
    );
    // From the moment a trader has signed the deposit, the two payouts, the escrow outputs of
    // both warning transactions and the inputs of the deposit transaction are watched.
    let mut expected_watch_list = vec![
        trade.buyer_payout().0,
        trade.seller_payout().0,
        OutPoint::new(txid(&ids.buyers_warning_tx_id), 0),
        OutPoint::new(txid(&ids.sellers_warning_tx_id), 0),
    ];
    expected_watch_list.extend(deposit_tx.input.iter().map(|input| input.previous_output));
    for party in [&trade.buyer, &trade.seller] {
        assert_eq!(party.chain.watched_outputs(), expected_watch_list);
    }

    let (maker_side, taker_side) = (trade.maker_side(), trade.taker_side());
    let mut makers_events = trade
        .party_mut(maker_side)
        .subscribe_tx_confirmation_status()
        .await
        .expect("SubscribeTxConfirmationStatus succeeds");
    let mut takers_events = trade.publish_deposit_tx().await;

    // The publisher sent the signed transaction to the network. The maker sent nothing.
    let takers_chain = Arc::clone(&trade.party(taker_side).chain);
    let makers_chain = Arc::clone(&trade.party(maker_side).chain);
    let mut sent = Vec::new();
    for _ in 0..100 {
        sent = takers_chain.sent();
        if !sent.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(sent.len(), 1, "the deposit transaction is sent once");
    let signed_deposit_tx = sent.remove(0);
    assert_eq!(signed_deposit_tx.compute_txid(), deposit_tx.compute_txid());
    assert!(
        signed_deposit_tx
            .input
            .iter()
            .all(|input| !input.witness.is_empty())
    );
    assert_eq!(makers_chain.sent(), vec![]);

    // Every coin of the mock wallets holds 1 BTC.
    let input_amount = Amount::ONE_BTC.to_sat() * signed_deposit_tx.input.len() as u64;
    let output_amount: u64 = signed_deposit_tx
        .output
        .iter()
        .map(|o| o.value.to_sat())
        .sum();
    let minimum_fee = fee(DEPOSIT_TX_FEE_RATE, signed_deposit_tx.weight().to_wu());
    let paid_fee = input_amount - output_amount;
    assert!(
        (minimum_fee..=minimum_fee + 1).contains(&paid_fee),
        "paid {paid_fee} sat, target {minimum_fee} sat"
    );

    // N2: a block without the transaction gives no event, and the publisher sends the
    // transaction again, because it does not know whether a node accepted it.
    takers_chain.mine(&[]);
    makers_chain.mine(&[]);
    for _ in 0..100 {
        if takers_chain.sent().len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(takers_chain.sent().len(), 2, "sent again after a block");

    // The block with the transaction, and two more blocks: one event per confirmation, for both
    // traders, with the height of the chain. The stream ends at 3 confirmations (D6).
    let deposit_height = FakeChain::START_HEIGHT + 2;
    for chain in [&takers_chain, &makers_chain] {
        chain.mine(std::slice::from_ref(&signed_deposit_tx));
    }
    for confirmations in 1..=3 {
        for events in [&mut takers_events, &mut makers_events] {
            let event = next_event(events).await.expect("an event");
            assert_eq!(tx(&event.tx), signed_deposit_tx);
            assert_eq!(event.num_confirmations, confirmations);
            assert_eq!(
                event.current_block_height,
                deposit_height + confirmations - 1
            );
        }
        for chain in [&takers_chain, &makers_chain] {
            chain.mine(&[]);
        }
    }
    for events in [&mut takers_events, &mut makers_events] {
        assert!(
            next_event(events).await.is_none(),
            "the stream ends when the deposit counts as confirmed"
        );
    }
    // The confirmed transaction is not sent again.
    assert_eq!(takers_chain.sent().len(), 2);
}

/// F3: `InitTrade` reports the height of the chain as the service sees it. A service without
/// access to the chain reports the height 0, can still set up a trade, and refuses to publish or
/// to observe the deposit transaction without a change of the trade.
#[tokio::test]
async fn chain_height_and_missing_chain_access() {
    let mut trade = Trade::new(TradeType::TakerIsBuyer).await;
    trade.init_trades().await;
    for party in [&trade.buyer, &trade.seller] {
        let key_shares = party.key_shares.as_ref().expect("message A/B");
        assert_eq!(key_shares.current_block_height, FakeChain::START_HEIGHT);
    }

    let (mut client, trades) = start_service_with_chain(true, None).await;
    let trade_id = format!("no-chain-{}", next_trade_number());
    let key_shares = client
        .init_trade(PubKeySharesRequest {
            trade_id: trade_id.clone(),
            my_role: Role::BuyerAsTaker.into(),
        })
        .await
        .expect("a trade can be set up without the chain")
        .into_inner();
    assert_eq!(key_shares.current_block_height, 0);

    let message = "no access to the chain";
    let result = client
        .publish_deposit_tx(PublishDepositTxRequest {
            trade_id: trade_id.clone(),
            peers_deposit_psbt: Some(DepositPsbt::default()),
        })
        .await;
    assert_refused(result, Code::FailedPrecondition, message);
    let result = client
        .subscribe_tx_confirmation_status(SubscribeTxConfirmationStatusRequest {
            trade_id: trade_id.clone(),
        })
        .await;
    assert_refused(result, Code::FailedPrecondition, message);
    let trade = trades.get_trade_model(&trade_id).expect("stored");
    let trade = trade.lock().unwrap();
    assert_eq!(trade.state(), TradeState::Init);
    assert_eq!(trade.termination_reason(), None);
}

/// Waits until the chain of a service has got `count` transactions to send.
async fn sent_count_reaches(chain: &FakeChain, count: usize) -> bool {
    for _ in 0..500 {
        if chain.sent().len() >= count {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// A trade in which both traders have signed the deposit transaction, and nobody has published
/// it.
async fn trade_with_signed_deposit() -> Trade {
    let mut trade = Trade::at(TradeType::TakerIsBuyer, Stage::SignaturesExchanged).await;
    trade.sign_deposit_tx().await;
    trade
}

/// J10 and J5 for the service, while the chain reports a problem with the peers:
/// - `PublishDepositTx` is refused with a status that names the problem. It changes nothing in the
///   trade and sends nothing. When the problem is over, the same request succeeds.
/// - `SubscribeTxConfirmationStatus` only observes, so it is accepted (D21). Its stream gives no
///   event while the problem lasts, and gives the events of the deposit afterwards.
#[tokio::test]
async fn j10_publishing_is_refused_and_observing_is_accepted_during_a_peer_problem() {
    for (problem, message) in [
        (
            PeerProblem::NoCompactFilters,
            "does not serve compact block filters",
        ),
        (PeerProblem::NoPeerReachable, "no peer"),
    ] {
        let mut trade = trade_with_signed_deposit().await;
        let (maker_side, taker_side) = (trade.maker_side(), trade.taker_side());
        let makers_deposit_psbt = trade.party(maker_side).deposit_psbt.clone().unwrap();
        for side in SIDES {
            trade.party(side).chain.set_peer_problem(Some(problem));
        }

        let result = trade
            .party_mut(taker_side)
            .publish_deposit_tx(&makers_deposit_psbt)
            .await;
        assert_refused(result, Code::Unavailable, message);
        let mut makers_events = trade
            .party_mut(maker_side)
            .subscribe_tx_confirmation_status()
            .await
            .expect("SubscribeTxConfirmationStatus is accepted during the problem");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), makers_events.next())
                .await
                .is_err(),
            "no event while the chain is not observed"
        );
        trade.assert_states(TradeState::Deposit, TradeState::Deposit);
        for side in SIDES {
            let party = trade.party(side);
            assert_eq!(party.chain.sent(), vec![], "nothing is sent");
            let stored = party.trades.get_trade_model(&party.trade_id).unwrap();
            assert_eq!(stored.lock().unwrap().termination_reason(), None);
        }

        for side in SIDES {
            trade.party(side).chain.set_peer_problem(None);
        }
        drop(trade.publish_deposit_tx().await);
        let takers_chain = Arc::clone(&trade.party(taker_side).chain);
        assert!(sent_count_reaches(&takers_chain, 1).await);
        // The stream that was opened during the problem reports the deposit when the maker's
        // chain sees it in a block.
        let deposit_tx = takers_chain.sent().remove(0);
        trade
            .party(maker_side)
            .chain
            .mine(std::slice::from_ref(&deposit_tx));
        let event = next_event(&mut makers_events).await.expect("an event");
        assert_eq!(event.num_confirmations, 1);
        assert_eq!(tx(&event.tx), deposit_tx);
    }
}

/// J5 for the service: the peers become unreachable after the deposit transaction was accepted
/// for publishing and before it could be sent. The service keeps trying, reports no
/// confirmation, and sends the transaction when a peer is reachable again.
#[tokio::test]
async fn j5_deposit_is_sent_when_a_peer_is_reachable_again() {
    let mut trade = trade_with_signed_deposit().await;
    let taker_side = trade.taker_side();
    let takers_chain = Arc::clone(&trade.party(taker_side).chain);
    // The check of the request passes, and the first sending fails.
    takers_chain.fail_broadcasts(true);
    let mut events = trade.publish_deposit_tx().await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(takers_chain.sent(), vec![]);
    assert!(takers_chain.failed_broadcasts() >= 1);
    trade.assert_states(TradeState::Deposit, TradeState::Deposit);

    // No new block arrives. The service sends again after its pause.
    takers_chain.fail_broadcasts(false);
    assert!(
        sent_count_reaches(&takers_chain, 1).await,
        "the deposit transaction is sent after the peer is back"
    );
    let deposit_tx = takers_chain.sent().remove(0);
    assert_eq!(deposit_tx.compute_txid(), trade.deposit_tx().compute_txid());

    takers_chain.mine(std::slice::from_ref(&deposit_tx));
    let event = next_event(&mut events).await.expect("an event");
    assert_eq!(event.num_confirmations, 1);
}

/// J9 for the service: a deposit transaction that no node accepts is sent again after every
/// block, as the schedule of N2 says, also when no peer asks for it. Nothing is reported as
/// confirmed, the trade stays in its state, and the sending ends after the deposit timeout.
#[tokio::test]
async fn j9_deposit_that_never_confirms_is_sent_on_schedule_and_never_reported() {
    let mut trade = trade_with_signed_deposit().await;
    let taker_side = trade.taker_side();
    let takers_chain = Arc::clone(&trade.party(taker_side).chain);
    let mut events = trade.publish_deposit_tx().await;
    assert!(sent_count_reaches(&takers_chain, 1).await);

    // The peer does not ask for a transaction that it has rejected since the last block.
    takers_chain.ignore_broadcasts(true);
    takers_chain.mine(&[]);
    for _ in 0..100 {
        if takers_chain.ignored_broadcasts() >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(takers_chain.ignored_broadcasts(), 1, "announced once");
    takers_chain.ignore_broadcasts(false);

    // One sending per block, up to the deposit timeout of the configuration.
    let deposit_timeout = usize::from(EngineConfig::default().deposit_timeout());
    for block in 2..=deposit_timeout {
        takers_chain.mine(&[]);
        assert!(
            sent_count_reaches(&takers_chain, block).await,
            "sent again after block {block}"
        );
    }
    // One sending at the start and one after every block of the timeout, less the one for
    // which no peer asked.
    assert_eq!(takers_chain.sent().len(), deposit_timeout);
    // After the timeout the sending ends.
    takers_chain.mine(&[]);
    takers_chain.mine(&[]);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(takers_chain.sent().len(), deposit_timeout);

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), events.next())
            .await
            .is_err(),
        "no confirmation event for a transaction that is in no block"
    );
    trade.assert_states(TradeState::Deposit, TradeState::Deposit);
}

/// Reads the next event of a trade, with a limit for the waiting time.
async fn next_trade_event(events: &mut Streaming<TradeEvent>) -> TradeEvent {
    tokio::time::timeout(std::time::Duration::from_secs(10), events.next())
        .await
        .expect("the stream answers in time")
        .expect("the stream is open")
        .expect("event without error")
}

/// Reads events of a trade until one has the type `event_type`.
async fn trade_event_of_type(
    events: &mut Streaming<TradeEvent>,
    event_type: TradeEventType,
) -> TradeEvent {
    loop {
        let event = next_trade_event(events).await;
        if event.r#type() == event_type {
            return event;
        }
    }
}

/// The last transaction that the chain of a service got to send, when it has got `count`.
async fn sent_tx(chain: &FakeChain, count: usize) -> Transaction {
    assert!(
        sent_count_reaches(chain, count).await,
        "the service sends transaction number {count}"
    );
    chain.sent().remove(count - 1)
}

/// C1, C2 and the event stream of a trade: the seller asks for the forced close, and his
/// service sends the swap transaction. When a block contains it, the buyer's service reads the
/// seller's key share from it and closes the buyer's trade, without a request of the caller.
#[tokio::test]
async fn c1_swap_tx_is_sent_and_the_buyer_closes_from_the_chain() {
    let mut trade = Trade::at(TradeType::TakerIsBuyer, Stage::SellerReleased).await;
    let mut buyers_events = trade.buyer.subscribe_trade_events().await.unwrap();
    let mut sellers_events = trade.seller.subscribe_trade_events().await.unwrap();
    // The first event of a stream is the current state.
    let first = next_trade_event(&mut buyers_events).await;
    assert_eq!(first.r#type(), TradeEventType::State);
    assert_eq!(first.state(), TradeStateKind::BuyerReadyToRelease);
    assert_eq!(first.trade_id, trade.buyer.trade_id);
    assert!(!first.r#final);
    let first = next_trade_event(&mut sellers_events).await;
    assert_eq!(first.state(), TradeStateKind::SellerReadyToRelease);

    let sellers_chain = Arc::clone(&trade.seller.chain);
    let sent_before = sellers_chain.sent().len();
    trade.seller_closes_by_force().await;
    let swap_tx = sent_tx(&sellers_chain, sent_before + 1).await;
    assert_eq!(consensus::serialize(&swap_tx), trade.swap_tx());
    let event = trade_event_of_type(&mut sellers_events, TradeEventType::State).await;
    assert_eq!(event.state(), TradeStateKind::TradeClosed);
    assert_eq!(event.closure_type(), ClosureKind::Forced);
    let event = trade_event_of_type(&mut sellers_events, TradeEventType::TxSent).await;
    assert_eq!(event.tx_kind(), TradeTxKind::SwapTx);
    assert_eq!(event.tx_id, Some(swap_tx.compute_txid().to_string()));

    // The buyer's service knows nothing of the swap transaction before a block contains it.
    assert_eq!(trade.buyer.state(), TradeState::BuyerReadyToRelease);
    trade.mine(std::slice::from_ref(&swap_tx));
    let event = trade_event_of_type(&mut buyers_events, TradeEventType::State).await;
    assert_eq!(event.state(), TradeStateKind::TradeClosed);
    assert_eq!(event.closure_type(), ClosureKind::Forced);
    assert_eq!(
        trade.buyer.state(),
        TradeState::TradeClosed(ClosureType::Forced)
    );

    // Final at 3 confirmations of the swap transaction.
    trade.mine(&[]);
    trade.mine(&[]);
    for party in [&trade.buyer, &trade.seller] {
        let stored = party.trades.get_trade_model(&party.trade_id).unwrap();
        for _ in 0..200 {
            if stored.lock().unwrap().is_final() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(stored.lock().unwrap().is_final());
    }
    // The confirmed transaction was sent once.
    assert_eq!(sellers_chain.sent().len(), sent_before + 1);
}

/// C4 (D7): the seller released and the buyer does not answer. The seller's service sends the
/// swap transaction on its own when the number of blocks before the buyer's warning transaction
/// becomes valid has fallen to the release safety margin.
#[tokio::test]
async fn c4_swap_tx_is_sent_without_a_request_at_the_safety_margin() {
    let trade = Trade::at(TradeType::TakerIsSeller, Stage::SellerReleased).await;
    let sellers_chain = Arc::clone(&trade.seller.chain);
    let sent_before = sellers_chain.sent().len();
    // The deposit has 3 confirmations. The margin of 6 blocks is reached at 14 confirmations.
    for _ in 3..13 {
        trade.mine(&[]);
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(sellers_chain.sent().len(), sent_before);
    assert_eq!(trade.seller.state(), TradeState::SellerReadyToRelease);

    trade.mine(&[]);
    let swap_tx = sent_tx(&sellers_chain, sent_before + 1).await;
    assert_eq!(
        trade.seller.state(),
        TradeState::TradeClosed(ClosureType::Forced)
    );
    assert_eq!(
        swap_tx.input[0].previous_output,
        trade.seller_payout().0,
        "the swap transaction spends the seller's payout"
    );
}

/// E1, E2, E14 through the service: `PublishWarningTx` is refused before the warning lock time
/// has passed. Afterwards the service of the trader sends the warning transaction, and the
/// service of the peer answers the confirmed warning with the redirect transaction, without a
/// request. Both trades close when the redirect is confirmed.
#[tokio::test]
async fn e1_e2_publish_warning_tx_and_redirect() {
    for buyer_warns in [true, false] {
        let mut trade = Trade::at(TradeType::TakerIsBuyer, Stage::DepositSigned).await;
        let (publisher_side, peer_side) = if buyer_warns {
            (Side::Buyer, Side::Seller)
        } else {
            (Side::Seller, Side::Buyer)
        };
        let result = trade.party_mut(publisher_side).publish_warning_tx().await;
        assert_refused(result, Code::FailedPrecondition, "valid in 17 blocks");
        trade.assert_states(TradeState::Deposit, TradeState::Deposit);

        for _ in 3..WARNING_LOCK_TIME {
            trade.mine(&[]);
        }
        let mut peers_events = trade
            .party_mut(peer_side)
            .subscribe_trade_events()
            .await
            .unwrap();
        let publishers_chain = Arc::clone(&trade.party(publisher_side).chain);
        let peers_chain = Arc::clone(&trade.party(peer_side).chain);
        let (publishers_sent, peers_sent) =
            (publishers_chain.sent().len(), peers_chain.sent().len());

        let response = trade
            .party_mut(publisher_side)
            .publish_warning_tx()
            .await
            .expect("the warning lock time has passed");
        let warning_tx = sent_tx(&publishers_chain, publishers_sent + 1).await;
        assert_eq!(consensus::serialize(&warning_tx), response.warning_tx);
        let ids = trade.contractual_tx_ids();
        let (warning_txid, redirect_txid, warning_state, closure) = if buyer_warns {
            (
                &ids.buyers_warning_tx_id,
                &ids.sellers_redirect_tx_id,
                TradeState::BuyersWarning,
                ClosureType::SellersRedirect,
            )
        } else {
            (
                &ids.sellers_warning_tx_id,
                &ids.buyers_redirect_tx_id,
                TradeState::SellersWarning,
                ClosureType::BuyersRedirect,
            )
        };
        assert_eq!(warning_tx.compute_txid(), txid(warning_txid));
        assert_eq!(trade.party(publisher_side).state(), warning_state);
        assert_eq!(trade.party(peer_side).state(), TradeState::Deposit);
        // A repeated request returns the same transaction.
        let repeated = trade.party_mut(publisher_side).publish_warning_tx().await;
        assert_eq!(repeated.unwrap().warning_tx, response.warning_tx);

        // The warning is confirmed: the peer's service sends the redirect on its own.
        trade.mine(std::slice::from_ref(&warning_tx));
        let redirect_tx = sent_tx(&peers_chain, peers_sent + 1).await;
        assert_eq!(redirect_tx.compute_txid(), txid(redirect_txid));
        assert_eq!(trade.party(peer_side).state(), warning_state);
        let event = trade_event_of_type(&mut peers_events, TradeEventType::TxSent).await;
        assert_eq!(event.tx_kind(), TradeTxKind::RedirectTx);

        trade.mine(std::slice::from_ref(&redirect_tx));
        loop {
            let event = trade_event_of_type(&mut peers_events, TradeEventType::State).await;
            if event.state() == TradeStateKind::TradeClosed {
                break;
            }
        }
        let closed = TradeState::TradeClosed(closure);
        for _ in 0..200 {
            if trade.party(publisher_side).state() == closed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(trade.party(publisher_side).state(), closed);
        assert_eq!(trade.party(peer_side).state(), closed);
        // A seller or a buyer in a closed trade cannot start a dispute.
        let result = trade.party_mut(peer_side).publish_warning_tx().await;
        assert_refused(result, Code::FailedPrecondition, "not allowed in state");
    }
}

/// D21: a problem with the peers and its end are reported as events of a trade, also to a
/// stream that was open before the problem. `PublishWarningTx` must send, so it is refused
/// while the problem lasts.
#[tokio::test]
async fn d21_problem_with_the_peers_is_an_event_of_the_trade() {
    let mut trade = Trade::at(TradeType::TakerIsBuyer, Stage::DepositSigned).await;
    let mut events = trade.buyer.subscribe_trade_events().await.unwrap();
    assert_eq!(
        next_trade_event(&mut events).await.state(),
        TradeStateKind::Deposit
    );

    trade
        .buyer
        .chain
        .set_peer_problem(Some(PeerProblem::NoPeerReachable));
    let event = trade_event_of_type(&mut events, TradeEventType::ChainProblem).await;
    assert!(
        event
            .message
            .as_deref()
            .is_some_and(|m| m.contains("no peer")),
        "{event:?}"
    );
    assert_eq!(event.state(), TradeStateKind::Deposit);
    let result = trade.buyer.publish_warning_tx().await;
    assert_refused(result, Code::Unavailable, "no peer");

    trade.buyer.chain.set_peer_problem(None);
    let event = trade_event_of_type(&mut events, TradeEventType::ChainProblemOver).await;
    assert_eq!(event.message, None);
}

impl Trade {
    /// Both traders stop their service and start a new one.
    async fn restart(&mut self) {
        self.buyer.restart().await;
        self.seller.restart().await;
    }
}

/// H2, H3: both services restart after every step of a trade that closes cooperatively. The
/// trade ends as without a restart. A request that is repeated after a restart gives the same
/// response.
#[tokio::test]
async fn h2_restart_after_every_step_of_a_cooperative_close() {
    for trade_type in TRADE_TYPES {
        let mut trade = Trade::at(trade_type, Stage::DepositSigned).await;
        trade.restart().await;
        trade.assert_states(TradeState::Deposit, TradeState::Deposit);
        // The watch list is set up again after the restart.
        for party in [&trade.buyer, &trade.seller] {
            assert!(!party.chain.watched_outputs().is_empty());
        }

        trade.buyer_starts_payment().await;
        let message_e = trade.payment_started.clone().expect("message E");
        trade.restart().await;
        trade.assert_states(TradeState::BuyerReadyToRelease, TradeState::Deposit);
        // H3: the response of the buyer's service was lost, and the caller repeats the request.
        trade.buyer_starts_payment().await;
        assert_eq!(trade.payment_started, Some(message_e));

        trade.seller_receives_payment_start().await;
        trade.restart().await;
        trade.release_payout().await;
        let message_f = trade.sellers_key_share();
        trade.restart().await;
        trade.assert_states(
            TradeState::BuyerReadyToRelease,
            TradeState::SellerReadyToRelease,
        );
        trade.release_payout().await;
        assert_eq!(trade.sellers_key_share(), message_f);

        trade.buyer_closes_cooperatively().await;
        let message_g = trade.buyers_key_share();
        trade.restart().await;
        trade.buyer_closes_cooperatively().await;
        assert_eq!(trade.buyers_key_share(), message_g);
        trade.seller_closes_cooperatively().await;
        trade.restart().await;
        let closed = TradeState::TradeClosed(ClosureType::Cooperative);
        trade.assert_states(closed, closed);
    }
}

/// H1, H8, K2: a restart during the setup. The nonces of the setup are lost, so the trade is
/// abandoned and its coins are released. Its ID stays used.
#[tokio::test]
async fn h1_restart_during_the_setup_abandons_the_trade() {
    let mut trade = Trade::at(TradeType::TakerIsBuyer, Stage::SignaturesExchanged).await;
    for party in [&trade.buyer, &trade.seller] {
        assert!(!party.wallet.0.lock().await.list_locked_utxos().is_empty());
    }
    trade.restart().await;
    let abandoned = TradeState::TradeClosed(ClosureType::Abandoned);
    trade.assert_states(abandoned, abandoned);
    for party in [&trade.buyer, &trade.seller] {
        assert_eq!(party.wallet.0.lock().await.list_locked_utxos(), vec![]);
    }
    // The setup cannot continue.
    let signatures = trade
        .seller
        .partial_signatures
        .clone()
        .expect("message C/D");
    let result = trade.buyer.sign_deposit_tx(&signatures).await;
    assert_refused(result, Code::FailedPrecondition, "not allowed in state");
    // The ID is not accepted for another trade.
    let result = trade
        .buyer
        .client
        .init_trade(PubKeySharesRequest {
            trade_id: trade.buyer.trade_id.clone(),
            my_role: Role::BuyerAsTaker.into(),
        })
        .await;
    assert_refused(result, Code::AlreadyExists, "already exists");
    // The abandoned trade is stored as abandoned: another restart changes nothing.
    trade.restart().await;
    trade.assert_states(abandoned, abandoned);
}

/// H5: the swap transaction is confirmed while the buyer's service does not run. After its
/// start the buyer's service finds the transaction, reads the key share and closes the trade.
#[tokio::test]
async fn h5_swap_tx_confirmed_while_the_service_did_not_run() {
    let mut trade = Trade::at(TradeType::TakerIsBuyer, Stage::SellerReleased).await;
    let sellers_chain = Arc::clone(&trade.seller.chain);
    let sent_before = sellers_chain.sent().len();
    trade.buyer.stop().await;
    trade.seller_closes_by_force().await;
    let swap_tx = sent_tx(&sellers_chain, sent_before + 1).await;
    trade.mine(std::slice::from_ref(&swap_tx));
    trade.mine(&[]);

    trade.buyer.start().await;
    let closed = TradeState::TradeClosed(ClosureType::Forced);
    for _ in 0..200 {
        if trade.buyer.state() == closed {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(trade.buyer.state(), closed);
    // The close is stored: it is there after another restart, before the service has looked at
    // the chain again.
    trade.buyer.restart().await;
    assert_eq!(trade.buyer.state(), closed);
}

/// H5 for the dispute path: the peer's warning transaction is confirmed while the service does
/// not run. After its start the service answers with the redirect transaction.
#[tokio::test]
async fn h5_warning_confirmed_while_the_service_did_not_run() {
    let mut trade = Trade::at(TradeType::TakerIsBuyer, Stage::DepositSigned).await;
    for _ in 3..WARNING_LOCK_TIME {
        trade.mine(&[]);
    }
    let buyers_chain = Arc::clone(&trade.buyer.chain);
    let sellers_chain = Arc::clone(&trade.seller.chain);
    let (buyers_sent, sellers_sent) = (buyers_chain.sent().len(), sellers_chain.sent().len());
    trade.seller.stop().await;
    trade
        .buyer
        .publish_warning_tx()
        .await
        .expect("the warning lock time has passed");
    let warning_tx = sent_tx(&buyers_chain, buyers_sent + 1).await;
    trade.mine(std::slice::from_ref(&warning_tx));
    // The buyer's decision is stored.
    trade.buyer.restart().await;
    assert_eq!(trade.buyer.state(), TradeState::BuyersWarning);

    trade.seller.start().await;
    let redirect_tx = sent_tx(&sellers_chain, sellers_sent + 1).await;
    let ids = trade.contractual_tx_ids();
    assert_eq!(
        redirect_tx.compute_txid(),
        txid(&ids.sellers_redirect_tx_id)
    );
    assert_eq!(trade.seller.state(), TradeState::BuyersWarning);
}

/// H6: a record that cannot be loaded. No trade is loaded, and the service refuses every trade
/// request with a status that names the trade and the reason.
#[tokio::test]
async fn h6_damaged_record_stops_the_trade_service() {
    let mut trade = Trade::at(TradeType::TakerIsBuyer, Stage::DepositSigned).await;
    let trade_id = trade.buyer.trade_id.clone();
    trade.buyer.stop().await;
    {
        let mut wallet = trade.buyer.wallet.0.lock().await;
        let mut records = wallet.trade_records().expect("records");
        let (_, record) = records.pop().expect("one trade");
        wallet
            .store_trade_record(&trade_id, &record[..record.len() / 2])
            .expect("stored");
        // A second trade with a valid record is not loaded either.
        wallet
            .store_trade_record("other-trade", &record)
            .expect("stored");
    }
    trade.buyer.start().await;
    assert!(trade.buyer.trades.get_trade_model(&trade_id).is_none());
    assert!(trade.buyer.trades.get_trade_model("other-trade").is_none());

    let message = format!("trade {trade_id}");
    let result = trade.buyer.subscribe_trade_events().await;
    assert_refused(result, Code::DataLoss, &message);
    let result = trade
        .buyer
        .client
        .init_trade(PubKeySharesRequest {
            trade_id: "new-trade".to_owned(),
            my_role: Role::BuyerAsTaker.into(),
        })
        .await;
    assert_refused(result, Code::DataLoss, "could not be loaded");
}

/// H8: after a restart the reservations in the wallet agree with the trades. A coin that no
/// trade reserves is released, and the coin of a trade is reserved again.
#[tokio::test]
async fn h8_reservations_agree_with_the_trades_after_a_restart() {
    let mut trade = Trade::at(TradeType::TakerIsBuyer, Stage::DepositSigned).await;
    let trade_id = trade.seller.trade_id.clone();
    let reserved = {
        let stored = trade.seller.trades.get_trade_model(&trade_id).unwrap();
        let reserved = stored.lock().unwrap().reserved_coins().to_vec();
        reserved
    };
    assert!(!reserved.is_empty());
    let stray: OutPoint = "658654575bbbeb46e965bd9eb37fd3be550a7e0fa2d64bc5f218763155602300:7"
        .parse()
        .unwrap();
    trade.seller.stop().await;
    {
        let mut wallet = trade.seller.wallet.0.lock().await;
        wallet.unlock_utxo(reserved[0]).expect("unlocked");
        wallet.lock_utxo(stray).expect("locked");
    }
    trade.seller.start().await;
    let mut locked = trade.seller.wallet.0.lock().await.list_locked_utxos();
    locked.sort();
    let mut expected = reserved;
    expected.sort();
    assert_eq!(locked, expected);
}

#[tokio::test]
async fn setup_and_deposit() {
    // Each run waits 5 seconds for the mock confirmation event, so both run at the same time.
    tokio::join!(
        check_setup_and_deposit(TradeType::TakerIsBuyer),
        check_setup_and_deposit(TradeType::TakerIsSeller),
    );
}

/// Repeated setup calls return the same messages as the first call (part of G12).
#[tokio::test]
async fn repeated_setup_calls_return_the_same_messages() {
    for trade_type in TRADE_TYPES {
        let mut trade = Trade::at(trade_type, Stage::SignaturesExchanged).await;
        for (side, peer_side) in [(Side::Buyer, Side::Seller), (Side::Seller, Side::Buyer)] {
            let peers_key_shares = trade
                .party(peer_side)
                .key_shares
                .clone()
                .expect("message A/B");
            let peers_nonce_shares = trade
                .party(peer_side)
                .nonce_shares
                .clone()
                .expect("message B/C");
            let party = trade.party_mut(side);

            let nonce_shares = party.get_nonce_shares(&peers_key_shares).await;
            assert_eq!(nonce_shares.ok(), party.nonce_shares);
            let partial_signatures = party.get_partial_signatures(&peers_nonce_shares).await;
            assert_eq!(partial_signatures.ok(), party.partial_signatures);
            assert_eq!(party.state(), TradeState::Init);
        }
    }
}

/// A trader who did not sign the deposit transaction cannot publish it, also if the peer's signed
/// PSBT is already there (part of K1). The request can be repeated after he signed (G15).
#[tokio::test]
async fn deposit_tx_is_not_published_before_it_is_signed() {
    for trade_type in TRADE_TYPES {
        let mut trade = Trade::at(trade_type, Stage::SignaturesExchanged).await;
        let (maker_side, taker_side) = (trade.maker_side(), trade.taker_side());

        let takers_signatures = trade
            .party(taker_side)
            .partial_signatures
            .clone()
            .expect("C");
        let makers_signatures = trade
            .party(maker_side)
            .partial_signatures
            .clone()
            .expect("D");
        let makers_deposit_psbt = trade
            .party_mut(maker_side)
            .sign_deposit_tx(&takers_signatures)
            .await
            .expect("maker signs the deposit transaction");

        let taker = trade.party_mut(taker_side);
        let result = taker.publish_deposit_tx(&makers_deposit_psbt).await;
        assert_not_allowed(result, taker, TradeState::Init);

        taker
            .sign_deposit_tx(&makers_signatures)
            .await
            .expect("taker signs the deposit transaction");
        taker
            .publish_deposit_tx(&makers_deposit_psbt)
            .await
            .expect("taker publishes the deposit transaction");
    }
}

// ---- B. Cooperative close ----

/// B1, as far as the service allows: the key shares are exchanged and both trades are closed.
/// Wallet balances cannot be checked, because the mock wallets do not import the payout key (F5).
#[tokio::test]
async fn b1_cooperative_close() {
    for trade_type in TRADE_TYPES {
        let mut trade = Trade::at(trade_type, Stage::SellerReleased).await;
        let ids = trade.contractual_tx_ids();
        let buyers_pub_key_shares = trade.buyer.key_shares.clone().expect("message A/B");
        let sellers_pub_key_shares = trade.seller.key_shares.clone().expect("message A/B");

        // Message F releases the buyer's payout: it is the seller's key share of that output.
        let sellers_key_share = trade.sellers_key_share();
        assert_key_pair(
            &sellers_key_share,
            &sellers_pub_key_shares.buyer_output_pub_key_share,
        );

        // Message G releases the seller's payout: it is the buyer's key share of that output.
        trade.buyer_closes_cooperatively().await;
        let buyers_response = trade.buyer.close_response.clone().expect("buyer closed");
        let buyers_key_share = trade.buyers_key_share();
        assert_key_pair(
            &buyers_key_share,
            &buyers_pub_key_shares.seller_output_pub_key_share,
        );
        assert_eq!(buyers_response.swap_tx, None);
        assert_penalty_tx(
            buyers_response.penalty_tx.as_ref(),
            &ids.sellers_warning_tx_id,
            &trade.claim_tx_payout_address(Side::Buyer),
            BUYERS_PENALTY_TX_FEE_RATE,
        );

        trade.seller_closes_cooperatively().await;
        let sellers_response = trade.seller.close_response.clone().expect("seller closed");
        assert_eq!(
            sellers_response.peer_output_prv_key_share,
            Some(sellers_key_share)
        );
        assert_eq!(sellers_response.swap_tx, None);
        // The seller's call set no penalty fee rate, so the prepared fee rate applies.
        assert_penalty_tx(
            sellers_response.penalty_tx.as_ref(),
            &ids.buyers_warning_tx_id,
            &trade.claim_tx_payout_address(Side::Seller),
            PREPARED_TX_FEE_RATE,
        );
    }
}

/// B3: the seller's service returns no key share before the buyer started the payment.
#[tokio::test]
async fn b3_seller_cannot_release_before_payment_started() {
    for trade_type in TRADE_TYPES {
        for (stage, state) in [
            (Stage::SignaturesExchanged, TradeState::Init),
            (Stage::DepositSigned, TradeState::Deposit),
            // The buyer started the payment, but message E did not reach the seller.
            (Stage::BuyerStartedPayment, TradeState::Deposit),
            (Stage::CustomPayoutSigned, TradeState::CustomPayoutSigned),
        ] {
            let mut trade = Trade::at(trade_type, stage).await;
            let result = trade.seller.release_payout().await;
            assert_not_allowed(result, &trade.seller, state);
        }
    }
}

/// B4: the buyer's service returns no key share before the seller released the payout, and the
/// buyer cannot use the calls of the seller to get one.
#[tokio::test]
async fn b4_buyer_gets_no_key_share_before_release() {
    for trade_type in TRADE_TYPES {
        for (stage, state) in [
            (Stage::SignaturesExchanged, TradeState::Init),
            (Stage::DepositSigned, TradeState::Deposit),
            (Stage::PaymentStarted, TradeState::BuyerReadyToRelease),
            (Stage::CustomPayoutSigned, TradeState::CustomPayoutSigned),
        ] {
            let mut trade = Trade::at(trade_type, stage).await;
            let buyer = &mut trade.buyer;

            let seller_only = "operation only available for seller";
            let result = buyer.close_trade_without_arguments().await;
            assert_refused(result, Code::FailedPrecondition, seller_only);
            let result = buyer.release_payout().await;
            assert_refused(result, Code::FailedPrecondition, seller_only);

            // A transaction that does not spend the seller's payout reveals no key share.
            let result = buyer.close_trade_with_swap_tx(&unrelated_tx()).await;
            match state {
                TradeState::Init | TradeState::Deposit => assert_not_allowed(result, buyer, state),
                _ => assert_refused(
                    result,
                    Code::FailedPrecondition,
                    "does not reveal the seller's key share",
                ),
            }
            assert_eq!(buyer.state(), state, "{stage:?}");
            assert!(!buyer.is_terminated(), "{stage:?}");

            // A key share that the seller did not create. Where the buyer may close, it proves a
            // modified client of the seller.
            let result = buyer.close_trade_with_key_share(&random_scalar()).await;
            if state == TradeState::BuyerReadyToRelease {
                assert_terminated(result, buyer, "does not match its public key share");
            } else {
                assert_not_allowed(result, buyer, state);
            }
            assert_eq!(buyer.state(), state, "{stage:?}");
        }
    }
}

/// The buyer's partial signature for the swap transaction leaves the buyer's service only after
/// he started the payment (belongs to B3 and C4).
#[tokio::test]
async fn buyers_swap_tx_signature_is_returned_only_after_payment_start() {
    for trade_type in TRADE_TYPES {
        // Before the deposit is signed, the buyer cannot start the payment.
        let mut trade = Trade::at(trade_type, Stage::SignaturesExchanged).await;
        let result = trade.buyer.start_payment().await;
        assert_not_allowed(result, &trade.buyer, TradeState::Init);

        // The signature that the seller needs is in message E and in every repetition of it,
        // also after the trade moved on.
        for stage in [
            Stage::PaymentStarted,
            Stage::ClosedCooperatively,
            Stage::CustomPayoutSignedAfterPaymentStart,
        ] {
            let mut trade = Trade::at(trade_type, stage).await;
            let state = trade.buyer.state();
            let repeated = trade
                .buyer
                .start_payment()
                .await
                .expect("repeated call succeeds");
            assert_eq!(
                repeated.swap_tx_input_partial_signature,
                Some(trade.buyers_swap_tx_signature()),
                "{stage:?}"
            );
            assert_eq!(trade.buyer.state(), state, "{stage:?}");
        }

        // A buyer who signed a custom payout does not start the payment any more.
        for stage in [Stage::CustomPayoutSigned, Stage::ClosedByCustomPayout] {
            let mut trade = Trade::at(trade_type, stage).await;
            let state = trade.buyer.state();
            let result = trade.buyer.start_payment().await;
            assert_not_allowed(result, &trade.buyer, state);
        }
    }
}

// ---- C. Forced close with the swap transaction ----

/// The first address of the mock wallet of the seller. The service uses it for the output of the
/// swap transaction.
//noinspection SpellCheckingInspection
const MOCK_SELLER_SWAP_TX_ADDRESS: &str =
    "bcrt1p80xu5f0nqjarfnechsmlt488jf3tykx8cva9zeeczlsu4c7x557qr499gz";

/// Checks that `swap_tx` is a valid spend of the seller's payout to the seller's wallet.
#[track_caller]
fn assert_swap_tx(trade: &Trade, swap_tx: &[u8]) {
    let swap_tx = tx(swap_tx);
    let (seller_payout, seller_payout_output) = trade.seller_payout();
    assert_eq!(swap_tx.input.len(), 1);
    assert_eq!(swap_tx.input[0].previous_output, seller_payout);
    assert_eq!(swap_tx.input[0].sequence, Sequence::ENABLE_RBF_NO_LOCKTIME);
    assert_eq!(
        swap_tx.output,
        [TxOut {
            value: Amount::from_sat(
                SELLERS_SECURITY_DEPOSIT - fee(PREPARED_TX_FEE_RATE, FORWARDING_TX_WEIGHT)
            ),
            script_pubkey: address(MOCK_SELLER_SWAP_TX_ADDRESS).script_pubkey(),
        }]
    );
    assert_valid_key_spend(&swap_tx, 0, &[seller_payout_output]);
}

/// C1 and C2, as far as the service allows without a chain: the seller's service returns a valid
/// swap transaction, and the buyer's service closes the trade with it. The service broadcasts
/// nothing and does not start the forced close on its own (F3, D7).
#[tokio::test]
async fn c1_forced_close_with_swap_tx() {
    for trade_type in TRADE_TYPES {
        let mut trade = Trade::at(trade_type, Stage::SellerReleased).await;
        let ids = trade.contractual_tx_ids();

        trade.seller_closes_by_force().await;
        let sellers_response = trade.seller.close_response.clone().expect("seller closed");
        assert_eq!(sellers_response.peer_output_prv_key_share, None);
        assert_eq!(sellers_response.penalty_tx, None);
        assert_swap_tx(&trade, &trade.swap_tx());

        // The swap transaction gives the buyer the seller's key share, so the buyer's service
        // can close the trade and sign the penalty transaction alone.
        trade.buyer_closes_with_swap_tx().await;
        let buyers_response = trade.buyer.close_response.clone().expect("buyer closed");
        assert_eq!(buyers_response.peer_output_prv_key_share, None);
        assert_eq!(buyers_response.swap_tx, None);
        // This call set no penalty fee rate, so the prepared fee rate applies.
        assert_penalty_tx(
            buyers_response.penalty_tx.as_ref(),
            &ids.sellers_warning_tx_id,
            &trade.claim_tx_payout_address(Side::Buyer),
            PREPARED_TX_FEE_RATE,
        );
    }
}

/// The last scenario of the Java client: the buyer signs a custom payout after the seller
/// released the payout. The seller cannot use it any more and closes with the swap transaction.
/// The buyer's trade then closes as a forced close (C1, D6 without a chain).
#[tokio::test]
async fn c1_forced_close_after_buyer_signed_custom_payout() {
    for trade_type in TRADE_TYPES {
        let mut trade = Trade::at(trade_type, Stage::SellerReleased).await;

        let custom_payout_psbt = trade
            .buyer
            .sign_custom_payout_tx(15_000)
            .await
            .expect("buyer signs the custom payout");
        assert_eq!(trade.buyer.state(), TradeState::CustomPayoutSigned);

        let state = TradeState::SellerReadyToRelease;
        let result = trade
            .seller
            .custom_close_trade(&custom_payout_psbt.psbt)
            .await;
        assert_not_allowed(result, &trade.seller, state);
        let result = trade.seller.sign_custom_payout_tx(15_000).await;
        assert_not_allowed(result, &trade.seller, state);

        trade.seller_closes_by_force().await;
        assert_swap_tx(&trade, &trade.swap_tx());
        trade.buyer_closes_with_swap_tx().await;
    }
}

/// C4: the seller's service returns no swap transaction before it has the buyer's partial
/// signature, and it accepts only the valid signature.
#[tokio::test]
async fn c4_seller_gets_no_swap_tx_before_payment_started() {
    for trade_type in TRADE_TYPES {
        for (stage, state) in [
            (Stage::SignaturesExchanged, TradeState::Init),
            (Stage::DepositSigned, TradeState::Deposit),
            (Stage::BuyerStartedPayment, TradeState::Deposit),
            // With the buyer's signature, the seller must still confirm the payment first.
            (Stage::PaymentStarted, TradeState::BuyerReadyToRelease),
        ] {
            let mut trade = Trade::at(trade_type, stage).await;
            let result = trade.seller.close_trade_without_arguments().await;
            assert_not_allowed(result, &trade.seller, state);
        }

        // Before the deposit is signed, the seller takes no signature of the buyer.
        let mut trade = Trade::at(trade_type, Stage::SignaturesExchanged).await;
        let result = trade.seller.sign_swap_tx(&random_scalar()).await;
        assert_not_allowed(result, &trade.seller, TradeState::Init);

        // A signature of the right form that is not the buyer's signature (G5), and a signature
        // that cannot be decoded, prove a modified client of the buyer.
        for invalid_signature in [random_scalar(), vec![]] {
            let mut trade = Trade::at(trade_type, Stage::BuyerStartedPayment).await;
            let result = trade.seller.sign_swap_tx(&invalid_signature).await;
            assert_terminated(result, &trade.seller, "");
            assert_eq!(trade.seller.state(), TradeState::Deposit);
        }
    }
}

/// G13: invalid data of the peer ends the use of the trade (D17). After an invalid partial
/// signature for the swap transaction, the seller's service refuses the valid one and every
/// further step of the cooperative close.
#[tokio::test]
async fn rejected_swap_tx_signature_blocks_the_trade() {
    for trade_type in TRADE_TYPES {
        let mut trade = Trade::at(trade_type, Stage::BuyerStartedPayment).await;
        let valid_signature = trade.buyers_swap_tx_signature();
        let reason = "the buyer's partial signature for the swap transaction is invalid";

        let result = trade.seller.sign_swap_tx(&random_scalar()).await;
        assert_terminated(result, &trade.seller, reason);
        let result = trade.seller.sign_swap_tx(&valid_signature).await;
        assert_terminated(result, &trade.seller, reason);
        let result = trade.seller.release_payout().await;
        assert_terminated(result, &trade.seller, reason);
        assert_eq!(trade.seller.state(), TradeState::Deposit);
    }
}

// ---- D. Custom payout after mediation ----

const CUSTOM_PAYOUT_TX_FEE: u64 = fee(CUSTOM_PAYOUT_TX_FEE_RATE, CUSTOM_PAYOUT_TX_WEIGHT);
/// The seller pays half of the fee, rounded down.
const SELLERS_CUSTOM_PAYOUT_OUTPUT_AMOUNT: u64 =
    SELLERS_CUSTOM_PAYOUT_AMOUNT - CUSTOM_PAYOUT_TX_FEE / 2;
/// The buyer receives the rest and pays the other half of the fee, rounded up.
const BUYERS_CUSTOM_PAYOUT_OUTPUT_AMOUNT: u64 =
    DEPOSIT_AMOUNT - SELLERS_CUSTOM_PAYOUT_AMOUNT - CUSTOM_PAYOUT_TX_FEE.div_ceil(2);

/// D1, as far as the service allows without a chain: both sides sign and finalise the same
/// transaction with the mediated outputs. The mock wallets sign with fixed signatures, so the
/// witnesses cannot be verified.
#[tokio::test]
async fn d1_custom_payout() {
    for trade_type in TRADE_TYPES {
        let mut trade = Trade::at(trade_type, Stage::CustomPayoutSigned).await;
        let buyers_psbt = trade
            .buyer
            .custom_payout_psbt
            .clone()
            .expect("buyer signed");
        let sellers_psbt = trade
            .seller
            .custom_payout_psbt
            .clone()
            .expect("seller signed");

        let expected_inputs = [trade.buyer_payout().0, trade.seller_payout().0];
        let expected_outputs = [
            TxOut {
                value: Amount::from_sat(BUYERS_CUSTOM_PAYOUT_OUTPUT_AMOUNT),
                script_pubkey: trade.claim_tx_payout_address(Side::Buyer).script_pubkey(),
            },
            TxOut {
                value: Amount::from_sat(SELLERS_CUSTOM_PAYOUT_OUTPUT_AMOUNT),
                script_pubkey: trade.claim_tx_payout_address(Side::Seller).script_pubkey(),
            },
        ];
        for custom_payout_psbt in [&buyers_psbt, &sellers_psbt] {
            let unsigned_tx = psbt(&custom_payout_psbt.psbt).unsigned_tx;
            assert_eq!(
                custom_payout_psbt.tx_id,
                unsigned_tx.compute_txid().to_string()
            );
            assert_eq!(
                custom_payout_psbt.buyers_payout_amount_including_fee,
                BUYERS_CUSTOM_PAYOUT_OUTPUT_AMOUNT
            );
            assert_eq!(
                custom_payout_psbt.sellers_payout_amount_including_fee,
                SELLERS_CUSTOM_PAYOUT_OUTPUT_AMOUNT
            );
            assert_eq!(unsigned_tx.output, expected_outputs);
            let inputs: Vec<_> = unsigned_tx
                .input
                .iter()
                .map(|i| i.previous_output)
                .collect();
            assert_eq!(inputs, expected_inputs);
        }
        assert_eq!(buyers_psbt.tx_id, sellers_psbt.tx_id);
        assert_eq!(
            BUYERS_CUSTOM_PAYOUT_OUTPUT_AMOUNT + SELLERS_CUSTOM_PAYOUT_OUTPUT_AMOUNT,
            DEPOSIT_AMOUNT - CUSTOM_PAYOUT_TX_FEE,
            "the outputs and the fee add up to the deposit"
        );

        let [buyers_response, sellers_response] = trade.close_by_custom_payout().await;
        assert_eq!(
            buyers_response, sellers_response,
            "both sides hold the same transaction"
        );
        let custom_payout_tx = tx(&sellers_response.custom_payout_tx);
        assert_eq!(
            custom_payout_tx.compute_txid().to_string(),
            sellers_psbt.tx_id
        );
        // Each input is spent by the script path: two signatures, the script, the control block.
        assert!(
            custom_payout_tx
                .input
                .iter()
                .all(|input| input.witness.len() == 4)
        );
    }
}

/// D3: with the signature of one side only, no side can finalise the transaction. The acting side
/// stays in `CustomPayoutSigned`. The warning path of the expected result is not reachable through
/// the service (F4).
#[tokio::test]
async fn d3_custom_payout_with_one_signature() {
    for trade_type in TRADE_TYPES {
        for (signer_side, other_side) in [(Side::Buyer, Side::Seller), (Side::Seller, Side::Buyer)]
        {
            let mut trade = Trade::at(trade_type, Stage::DepositSigned).await;
            let custom_payout_psbt = trade
                .party_mut(signer_side)
                .sign_custom_payout_tx(SELLERS_CUSTOM_PAYOUT_AMOUNT)
                .await
                .expect("one side signs the custom payout");

            // The side that did not sign cannot finalise the PSBT of the signer.
            let other = trade.party_mut(other_side);
            let result = other.custom_close_trade(&custom_payout_psbt.psbt).await;
            assert_not_allowed(result, other, TradeState::Deposit);

            // The signer cannot finalise a PSBT that lacks the signature of the peer.
            let signer = trade.party_mut(signer_side);
            let result = signer.custom_close_trade(&custom_payout_psbt.psbt).await;
            assert_refused(
                result,
                Code::FailedPrecondition,
                "does not contain its signatures",
            );
            assert_eq!(signer.state(), TradeState::CustomPayoutSigned);
            assert!(!signer.is_terminated());
        }
    }
}

/// D5: a PSBT of the peer with other outputs or amounts is rejected.
#[tokio::test]
async fn d5_custom_payout_with_changed_peer_psbt() {
    for trade_type in TRADE_TYPES {
        // The two sides sign different amounts.
        let mut trade = Trade::at(trade_type, Stage::DepositSigned).await;
        let mut psbts = Vec::new();
        for (side, amount) in [(Side::Buyer, 245_000), (Side::Seller, 244_000)] {
            let party = trade.party_mut(side);
            psbts.push(
                party
                    .sign_custom_payout_tx(amount)
                    .await
                    .expect("side signs")
                    .psbt,
            );
        }
        for (side, peers_psbt) in [(Side::Buyer, &psbts[1]), (Side::Seller, &psbts[0])] {
            let party = trade.party_mut(side);
            let result = party.custom_close_trade(peers_psbt).await;
            assert_refused(result, Code::FailedPrecondition, "is for the transaction");
            assert_eq!(party.state(), TradeState::CustomPayoutSigned);
        }

        // Both sides sign the same amounts, but the PSBT is changed on its way to the peer.
        let mut trade = Trade::at(trade_type, Stage::CustomPayoutSigned).await;
        for (side, peer_side) in [(Side::Buyer, Side::Seller), (Side::Seller, Side::Buyer)] {
            let peers_psbt = trade
                .party(peer_side)
                .custom_payout_psbt
                .clone()
                .expect("signed")
                .psbt;
            let other_script = address(TRADE_FEE_RECEIVER.0).script_pubkey();

            let mut changed_amounts = psbt(&peers_psbt);
            changed_amounts.unsigned_tx.output[0].value -= Amount::from_sat(1_000);
            changed_amounts.unsigned_tx.output[1].value += Amount::from_sat(1_000);
            let mut changed_output = psbt(&peers_psbt);
            changed_output.unsigned_tx.output[0].script_pubkey = other_script;

            let party = trade.party_mut(side);
            for changed_psbt in [changed_amounts, changed_output] {
                let result = party.custom_close_trade(&changed_psbt.serialize()).await;
                assert_refused(result, Code::FailedPrecondition, "is for the transaction");
                assert_eq!(party.state(), TradeState::CustomPayoutSigned);
            }
        }
        // A PSBT for another transaction can have a harmless cause, for example another fee rate
        // of the peer's client. It is refused, and the trade continues (D17).
        trade.close_by_custom_payout().await;
    }
}

// ---- G. Invalid peer messages ----

/// G1 and G4, encoding part: key shares and nonces that are no valid points cannot be decoded.
/// An unmodified client never sends such a field, so the trade is terminated (D17). The remaining
/// cases of G1 and G4 are unit tests of the engine.
#[tokio::test]
async fn g1_g4_invalid_encodings_are_rejected() {
    type Change = fn(&mut PubKeySharesResponse, &[u8]);
    // 33 bytes with a valid prefix, but the X coordinate is not smaller than the field size.
    let no_point = [&[2][..], &[0xff; 32]].concat();
    let changes: [Change; 3] = [
        |key_shares, no_point| key_shares.buyer_output_pub_key_share = no_point.to_vec(),
        |key_shares, no_point| key_shares.seller_output_pub_key_share = no_point[..32].to_vec(),
        |key_shares, _| key_shares.multisig_script_key = vec![0xff; 32],
    ];
    for (side, peer_side) in [(Side::Buyer, Side::Seller), (Side::Seller, Side::Buyer)] {
        for change in changes {
            let mut trade = Trade::new(TradeType::TakerIsBuyer).await;
            trade.init_trades().await;
            let mut invalid_key_shares = trade
                .party(peer_side)
                .key_shares
                .clone()
                .expect("message A/B");
            change(&mut invalid_key_shares, &no_point);

            let party = trade.party_mut(side);
            let result = party.get_nonce_shares(&invalid_key_shares).await;
            assert_terminated(result, party, "could not decode");
            assert_eq!(party.state(), ABANDONED);
        }

        let mut trade = Trade::new(TradeType::TakerIsBuyer).await;
        trade.init_trades().await;
        trade.exchange_nonce_shares().await;
        let peers_nonce_shares = trade
            .party(peer_side)
            .nonce_shares
            .clone()
            .expect("message B/C");
        // 66 bytes are the right length for a nonce, but they are no pair of points.
        let invalid_nonce_shares = NonceSharesMessage {
            buyers_warning_tx_buyer_input_nonce_share: vec![0xff; 66],
            ..peers_nonce_shares
        };
        let party = trade.party_mut(side);
        let result = party.get_partial_signatures(&invalid_nonce_shares).await;
        assert_terminated(result, party, "could not decode pub nonce");
        assert_eq!(party.state(), ABANDONED);
    }
}

/// G4: nonce shares of the peer in which one nonce is used for two signatures are rejected. The
/// rejection terminates the trade (D17): the valid nonce shares are refused afterwards.
#[tokio::test]
async fn g4_repeated_nonce_is_rejected() {
    type Change = fn(&mut NonceSharesMessage, &NonceSharesMessage);
    let changes: [Change; 2] = [
        // The peer uses one nonce for both warning transactions.
        |peers, _| {
            peers.sellers_warning_tx_buyer_input_nonce_share =
                peers.buyers_warning_tx_buyer_input_nonce_share.clone();
        },
        // The peer sends our own nonce back.
        |peers, own| peers.swap_tx_input_nonce_share = own.swap_tx_input_nonce_share.clone(),
    ];
    for trade_type in TRADE_TYPES {
        for (side, peer_side) in [(Side::Buyer, Side::Seller), (Side::Seller, Side::Buyer)] {
            for change in changes {
                let mut trade = Trade::new(trade_type).await;
                trade.init_trades().await;
                trade.exchange_nonce_shares().await;
                let own_nonce_shares = trade.party(side).nonce_shares.clone().expect("B/C");
                let peers_nonce_shares = trade.party(peer_side).nonce_shares.clone().expect("B/C");
                let mut repeated_nonce_shares = peers_nonce_shares.clone();
                change(&mut repeated_nonce_shares, &own_nonce_shares);

                let party = trade.party_mut(side);
                let result = party.get_partial_signatures(&repeated_nonce_shares).await;
                assert_terminated(result, party, "the peer's nonce share for the");
                let result = party.get_partial_signatures(&peers_nonce_shares).await;
                assert_terminated(result, party, "the peer's nonce share for the");
                assert_eq!(party.state(), ABANDONED);
            }
        }
    }
}

/// G10: a private key share that does not match the public key share of the peer is rejected, and
/// the state does not change. An unmodified client never sends such a key share, so the trade is
/// terminated (D17).
#[tokio::test]
async fn g10_wrong_key_share_at_close() {
    let mismatch = "does not match its public key share";
    for trade_type in TRADE_TYPES {
        // The buyer receives a wrong message F: another key share, or a zero scalar, which is no
        // valid key share.
        for (wrong_key_share, reason) in [
            (random_scalar(), mismatch),
            (vec![0; 32], "could not decode nonzero scalar"),
        ] {
            let mut trade = Trade::at(trade_type, Stage::SellerReleased).await;
            let result = trade
                .buyer
                .close_trade_with_key_share(&wrong_key_share)
                .await;
            assert_terminated(result, &trade.buyer, reason);
            assert_eq!(trade.buyer.state(), TradeState::BuyerReadyToRelease);
        }

        // The seller receives a wrong message G: another key share, or his own key share.
        for use_own_key_share in [false, true] {
            let mut trade = Trade::at(trade_type, Stage::BuyerClosedCooperatively).await;
            let wrong_key_share = if use_own_key_share {
                trade.sellers_key_share()
            } else {
                random_scalar()
            };
            let result = trade
                .seller
                .close_trade_with_key_share(&wrong_key_share)
                .await;
            assert_terminated(result, &trade.seller, mismatch);
            assert_eq!(trade.seller.state(), TradeState::SellerReadyToRelease);
        }
    }
}

/// G13: data of the peer that an unmodified client never produces terminates the trade (D17). The
/// valid data is refused afterwards, and the state does not change.
#[tokio::test]
async fn g13_invalid_peer_data_terminates_the_trade() {
    for trade_type in TRADE_TYPES {
        // A public key share that is no point of the curve.
        let mut trade = Trade::new(trade_type).await;
        trade.init_trades().await;
        let sellers_key_shares = trade.seller.key_shares.clone().expect("message A/B");
        let invalid_key_shares = PubKeySharesResponse {
            buyer_output_pub_key_share: [&[2][..], &[0xff; 32]].concat(),
            ..sellers_key_shares.clone()
        };
        let result = trade.buyer.get_nonce_shares(&invalid_key_shares).await;
        assert_terminated(result, &trade.buyer, "could not decode");
        let result = trade.buyer.get_nonce_shares(&sellers_key_shares).await;
        assert_terminated(result, &trade.buyer, "could not decode");
        assert_eq!(trade.buyer.state(), ABANDONED);

        // An invalid partial signature during the setup.
        let mut trade = Trade::at(trade_type, Stage::SignaturesExchanged).await;
        let sellers_signatures = trade.seller.partial_signatures.clone().expect("C/D");
        let invalid_signatures = PartialSignaturesMessage {
            peers_redirect_tx_input_partial_signature: random_scalar(),
            ..sellers_signatures.clone()
        };
        let reason = "a partial signature of the peer is not valid";
        let result = trade.buyer.sign_deposit_tx(&invalid_signatures).await;
        assert_terminated(result, &trade.buyer, reason);
        let result = trade.buyer.sign_deposit_tx(&sellers_signatures).await;
        assert_terminated(result, &trade.buyer, reason);
        assert_eq!(trade.buyer.state(), ABANDONED);

        // A wrong private key share in message F.
        let reason = "does not match its public key share";
        let mut trade = Trade::at(trade_type, Stage::SellerReleased).await;
        let sellers_key_share = trade.sellers_key_share();
        let result = trade
            .buyer
            .close_trade_with_key_share(&random_scalar())
            .await;
        assert_terminated(result, &trade.buyer, reason);
        let result = trade
            .buyer
            .close_trade_with_key_share(&sellers_key_share)
            .await;
        assert_terminated(result, &trade.buyer, reason);
        assert_eq!(trade.buyer.state(), TradeState::BuyerReadyToRelease);

        // A wrong private key share in message G.
        let mut trade = Trade::at(trade_type, Stage::BuyerClosedCooperatively).await;
        let buyers_key_share = trade.buyers_key_share();
        let result = trade
            .seller
            .close_trade_with_key_share(&random_scalar())
            .await;
        assert_terminated(result, &trade.seller, reason);
        let result = trade
            .seller
            .close_trade_with_key_share(&buyers_key_share)
            .await;
        assert_terminated(result, &trade.seller, reason);
        assert_eq!(trade.seller.state(), TradeState::SellerReadyToRelease);

        // A custom payout PSBT of the peer that cannot be decoded.
        let mut trade = Trade::at(trade_type, Stage::CustomPayoutSigned).await;
        let buyers_psbt = trade.buyer.custom_payout_psbt.clone().expect("signed").psbt;
        let result = trade.seller.custom_close_trade(b"no PSBT").await;
        assert_terminated(result, &trade.seller, "could not decode PSBT");
        let result = trade.seller.custom_close_trade(&buyers_psbt).await;
        assert_terminated(result, &trade.seller, "could not decode PSBT");
        assert_eq!(trade.seller.state(), TradeState::CustomPayoutSigned);

        // A second message of the peer with other data than the first one (G12).
        let mut trade = Trade::at(trade_type, Stage::PaymentStarted).await;
        let result = trade.seller.sign_swap_tx(&random_scalar()).await;
        assert_terminated(
            result,
            &trade.seller,
            "differs from the one that he sent before",
        );
        assert_eq!(trade.seller.state(), TradeState::BuyerReadyToRelease);
    }
}

/// G13 and D18: the preconditions of a request are checked before the data of the peer. Data that
/// cannot be decoded, in a request that is not allowed, does not terminate the trade.
#[tokio::test]
async fn g13_invalid_peer_data_in_a_request_that_is_not_allowed() {
    for trade_type in TRADE_TYPES {
        let mut trade = Trade::at(trade_type, Stage::SignaturesExchanged).await;
        let no_psbt = DepositPsbt {
            deposit_psbt: b"no PSBT".to_vec(),
        };
        for party in [&mut trade.buyer, &mut trade.seller] {
            let result = party.publish_deposit_tx(&no_psbt).await;
            assert_not_allowed(result, party, TradeState::Init);
            let result = party.close_trade_with_key_share(&[1, 2, 3]).await;
            assert_not_allowed(result, party, TradeState::Init);
            let result = party.custom_close_trade(b"no PSBT").await;
            assert_not_allowed(result, party, TradeState::Init);
        }
        let result = trade.seller.sign_swap_tx(&[1, 2, 3]).await;
        assert_not_allowed(result, &trade.seller, TradeState::Init);
        let result = trade.buyer.sign_swap_tx(&[1, 2, 3]).await;
        assert_refused(
            result,
            Code::FailedPrecondition,
            "only available for seller",
        );
        assert!(!trade.buyer.is_terminated());

        // The trade continues: both sides sign the deposit transaction.
        trade.sign_deposit_tx().await;
        trade.assert_states(TradeState::Deposit, TradeState::Deposit);
    }
}

/// A2: a trade can be abandoned while the trader has not signed the deposit transaction. An
/// abandoned trade accepts no request that would continue it. After the deposit transaction is
/// signed, the request is refused, because the peer can publish the deposit.
#[tokio::test]
async fn a2_abandon_trade() {
    for trade_type in TRADE_TYPES {
        let mut trade = Trade::at(trade_type, Stage::SignaturesExchanged).await;
        for (side, peers_side) in [(Side::Buyer, Side::Seller), (Side::Seller, Side::Buyer)] {
            let peers_signatures = trade
                .party(peers_side)
                .partial_signatures
                .clone()
                .expect("C/D");
            let party = trade.party_mut(side);
            party.abandon_trade().await.expect("allowed in Init");
            assert_eq!(party.state(), ABANDONED);
            party.abandon_trade().await.expect("repeated request");

            // The trade cannot continue: the deposit transaction is not signed any more.
            let result = party.sign_deposit_tx(&peers_signatures).await;
            assert_not_allowed(result, party, ABANDONED);
        }

        for stage in [
            Stage::DepositSigned,
            Stage::PaymentStarted,
            Stage::ClosedCooperatively,
        ] {
            let mut trade = Trade::at(trade_type, stage).await;
            for party in [&mut trade.buyer, &mut trade.seller] {
                let state = party.state();
                let result = party.abandon_trade().await;
                assert_not_allowed(result, party, state);
            }
        }
    }
}

/// G15: a request that is refused for a reason that an honest peer and a correct network can also
/// cause does not end the use of the trade. The trade closes normally afterwards (D17).
#[tokio::test]
async fn g15_refused_request_can_be_repeated() {
    for trade_type in TRADE_TYPES {
        let mut trade = Trade::at(trade_type, Stage::DepositSigned).await;

        // Requests that come too early, or from the wrong role.
        let result = trade.seller.release_payout().await;
        assert_not_allowed(result, &trade.seller, TradeState::Deposit);
        let result = trade.seller.close_trade_without_arguments().await;
        assert_not_allowed(result, &trade.seller, TradeState::Deposit);
        let result = trade.seller.start_payment().await;
        assert_refused(result, Code::FailedPrecondition, "only available for buyer");
        let result = trade.buyer.close_trade_without_arguments().await;
        assert_refused(
            result,
            Code::FailedPrecondition,
            "only available for seller",
        );
        let result = trade.buyer.release_payout().await;
        assert_refused(
            result,
            Code::FailedPrecondition,
            "only available for seller",
        );
        trade.assert_states(TradeState::Deposit, TradeState::Deposit);

        // Invalid data that the caller itself supplied: a fee rate that makes an output dust.
        let request = CustomPayoutPsbtRequest {
            trade_id: trade.buyer.trade_id.clone(),
            sellers_payout_amount_excluding_fee: 100,
            fee_rate: CUSTOM_PAYOUT_TX_FEE_RATE,
        };
        assert!(
            trade
                .buyer
                .client
                .sign_custom_payout_tx(request)
                .await
                .is_err()
        );
        trade.assert_states(TradeState::Deposit, TradeState::Deposit);
        assert!(!trade.buyer.is_terminated());

        // Requests that are repeated with the same data. The response of the publisher of the
        // deposit now also contains the signatures of the peer, so only the transactions are
        // compared.
        let deposit_tx = trade.deposit_tx();
        for (side, peer_side) in [(Side::Buyer, Side::Seller), (Side::Seller, Side::Buyer)] {
            let peers_signatures = trade
                .party(peer_side)
                .partial_signatures
                .clone()
                .expect("message C/D");
            let repeated = trade
                .party_mut(side)
                .sign_deposit_tx(&peers_signatures)
                .await
                .expect("repeated SignDepositTx succeeds");
            assert_eq!(psbt(&repeated.deposit_psbt).unsigned_tx, deposit_tx);
        }
        trade.assert_states(TradeState::Deposit, TradeState::Deposit);

        trade.buyer_starts_payment().await;
        trade.seller_receives_payment_start().await;
        trade.release_payout().await;
        trade.buyer_closes_cooperatively().await;
        trade.seller_closes_cooperatively().await;
    }
}

// ---- K. Interface misuse ----

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    /// The call succeeded and did not change the state.
    Accepted,
    /// The call succeeded and changed the state.
    MovedTo(TradeState),
    /// The call returned an error and did not change the state.
    Refused,
}

/// The state that a call tries to reach, if it tries to change the state.
const fn target_state(call: Call) -> Option<TradeState> {
    Some(match call {
        Call::GetNonceShares
        | Call::GetPartialSignatures
        | Call::PublishDepositTx
        | Call::SubscribeTxConfirmationStatus => return None,
        Call::SignDepositTx => TradeState::Deposit,
        Call::StartPayment | Call::SignSwapTx => TradeState::BuyerReadyToRelease,
        Call::ReleasePayout => TradeState::SellerReadyToRelease,
        Call::CloseTradeWithKeyShare => TradeState::TradeClosed(ClosureType::Cooperative),
        Call::CloseTradeWithSwapTx | Call::CloseTradeWithoutArguments => {
            TradeState::TradeClosed(ClosureType::Forced)
        }
        Call::SignCustomPayoutTx => TradeState::CustomPayoutSigned,
        Call::CustomCloseTrade => TradeState::TradeClosed(ClosureType::Custom),
    })
}

/// The result of every call in every stage, as the service behaves today.
///
/// A call that is repeated in a later state succeeds without a state change (`Accepted`). The
/// calls without a state check also succeed in every state after `Init`.
fn expected_outcome(side: Side, stage: Stage, call: Call) -> Outcome {
    use Outcome::{Accepted, MovedTo, Refused};
    use TradeState::{BuyerReadyToRelease, CustomPayoutSigned, Deposit, SellerReadyToRelease};
    let closed = |closure_type| MovedTo(TradeState::TradeClosed(closure_type));

    match (call, side) {
        // These calls have no state check. They return the stored data again.
        (Call::GetNonceShares | Call::GetPartialSignatures, _) => Accepted,
        (Call::SignDepositTx, _) => match stage {
            Stage::SignaturesExchanged => MovedTo(Deposit),
            _ => Accepted,
        },
        // The deposit transaction can be published and observed when the trader has signed it.
        (Call::PublishDepositTx | Call::SubscribeTxConfirmationStatus, _) => match stage {
            Stage::SignaturesExchanged => Refused,
            _ => Accepted,
        },
        (Call::StartPayment, Side::Buyer) => match stage {
            // A buyer who signed a custom payout directly after the deposit does not start the
            // payment any more.
            Stage::SignaturesExchanged
            | Stage::CustomPayoutSigned
            | Stage::ClosedByCustomPayout => Refused,
            Stage::DepositSigned => MovedTo(BuyerReadyToRelease),
            _ => Accepted,
        },
        // Calls of the other role.
        (
            Call::SignSwapTx | Call::ReleasePayout | Call::CloseTradeWithoutArguments,
            Side::Buyer,
        )
        | (Call::StartPayment | Call::CloseTradeWithSwapTx, Side::Seller) => Refused,
        (Call::SignSwapTx, Side::Seller) => match stage {
            Stage::BuyerStartedPayment => MovedTo(BuyerReadyToRelease),
            // The buyer did not create message E in these stages.
            Stage::SignaturesExchanged
            | Stage::DepositSigned
            | Stage::CustomPayoutSigned
            | Stage::ClosedByCustomPayout => Refused,
            _ => Accepted,
        },
        (Call::ReleasePayout, Side::Seller) => match stage {
            Stage::PaymentStarted => MovedTo(SellerReadyToRelease),
            Stage::SellerReleased
            | Stage::BuyerClosedCooperatively
            | Stage::ClosedCooperatively => Accepted,
            _ => Refused,
        },
        (Call::CloseTradeWithKeyShare, Side::Buyer) => match stage {
            Stage::SellerReleased | Stage::SellerClosedByForce => closed(ClosureType::Cooperative),
            Stage::BuyerClosedCooperatively | Stage::ClosedCooperatively => Accepted,
            _ => Refused,
        },
        (Call::CloseTradeWithKeyShare, Side::Seller) => match stage {
            Stage::BuyerClosedCooperatively => closed(ClosureType::Cooperative),
            Stage::ClosedCooperatively => Accepted,
            _ => Refused,
        },
        (Call::CloseTradeWithSwapTx, Side::Buyer) => match stage {
            Stage::SellerClosedByForce => closed(ClosureType::Forced),
            Stage::ClosedByForce => Accepted,
            _ => Refused,
        },
        (Call::CloseTradeWithoutArguments, Side::Seller) => match stage {
            Stage::SellerReleased | Stage::BuyerClosedCooperatively => closed(ClosureType::Forced),
            Stage::SellerClosedByForce | Stage::ClosedByForce => Accepted,
            _ => Refused,
        },
        (Call::SignCustomPayoutTx, _) => match (stage, side) {
            (Stage::DepositSigned | Stage::BuyerStartedPayment | Stage::PaymentStarted, _)
            | (Stage::SellerReleased | Stage::SellerClosedByForce, Side::Buyer) => {
                MovedTo(CustomPayoutSigned)
            }
            (
                Stage::CustomPayoutSigned
                | Stage::CustomPayoutSignedAfterPaymentStart
                | Stage::ClosedByCustomPayout,
                _,
            ) => Accepted,
            _ => Refused,
        },
        (Call::CustomCloseTrade, _) => match stage {
            Stage::CustomPayoutSigned | Stage::CustomPayoutSignedAfterPaymentStart => {
                closed(ClosureType::Custom)
            }
            Stage::ClosedByCustomPayout => Accepted,
            _ => Refused,
        },
    }
}

/// Performs every call once for each side in the given stage, each time with a new trade, and
/// returns a description of every result that is not the expected one.
async fn check_calls_at(trade_type: TradeType, stage: Stage) -> Vec<String> {
    let mut differences = Vec::new();
    for call in CALLS {
        let mut trade = Trade::at(trade_type, stage).await;
        for side in SIDES {
            let context = format!("{trade_type:?}, {stage:?}, {side:?}, {call:?}");
            let before = trade.party(side).state();
            let result = trade.call(side, call).await;
            let after = trade.party(side).state();

            let outcome = match (&result, before == after) {
                (Ok(()), true) => Outcome::Accepted,
                (Ok(()), false) => Outcome::MovedTo(after),
                (Err(_), true) => Outcome::Refused,
                (Err(status), false) => {
                    differences.push(format!("{context}: state {after} after error {status}"));
                    continue;
                }
            };
            let expected = expected_outcome(side, stage, call);
            if outcome != expected {
                differences.push(format!(
                    "{context}: expected {expected:?}, got {outcome:?} (result: {result:?})"
                ));
            }

            // Compare the result with the transition table of `protocol::state`.
            let is_allowed = |new_state| match side {
                Side::Buyer => before.precedes_for_buyer(new_state),
                Side::Seller => before.precedes_for_seller(new_state),
            };
            if let Outcome::MovedTo(new_state) = outcome {
                if !is_allowed(new_state) {
                    differences.push(format!("{context}: {before} -> {new_state} is not allowed"));
                }
            }
            if let Some(target_state) = target_state(call) {
                let is_repetition = before >= target_state;
                if !is_repetition && !is_allowed(target_state) && outcome != Outcome::Refused {
                    differences.push(format!(
                        "{context}: {before} -> {target_state} is not allowed, got {outcome:?}"
                    ));
                }
            }
        }
    }
    differences
}

/// K1 for the nine calls that need an existing trade: every call in every stage that the service
/// can reach, for all four roles. The states `BuyersWarning` and `SellersWarning` and the six
/// closure types after them cannot be reached through the service (F4), so they are not covered.
#[tokio::test(flavor = "multi_thread")]
async fn k1_every_call_in_every_stage() {
    let mut checks = Vec::new();
    for trade_type in TRADE_TYPES {
        for stage in STAGES {
            checks.push(tokio::spawn(check_calls_at(trade_type, stage)));
        }
    }
    let mut differences = Vec::new();
    for check in checks {
        differences.extend(check.await.expect("check completes"));
    }
    assert!(
        differences.is_empty(),
        "unexpected results:\n{}",
        differences.join("\n")
    );
}

/// K1 for `InitTrade`: an unknown role is rejected and no trade is stored.
#[tokio::test]
async fn k1_init_trade_with_unknown_role() {
    let (mut client, trades, _chain) = start_service(true).await;
    let trade_id = format!("unknown-role-{}", next_trade_number());
    let request = PubKeySharesRequest {
        trade_id: trade_id.clone(),
        my_role: 4,
    };
    assert_refused(
        client.init_trade(request).await,
        Code::OutOfRange,
        "unknown enum value: 4",
    );
    assert!(!trades.contains_trade(&trade_id));
}

/// K2, first part: every call for an unknown trade ID is rejected and stores no trade.
#[tokio::test]
async fn k2_unknown_trade_id() {
    let trade_id = format!("unknown-trade-{}", next_trade_number());
    let mut party = Party::new(trade_id.clone(), Role::BuyerAsTaker).await;
    let message = format!("missing trade with id: {trade_id}");

    // The contents of the requests do not matter, because the trade is looked up first.
    let result = party
        .get_nonce_shares(&PubKeySharesResponse::default())
        .await;
    assert_refused(result, Code::NotFound, &message);
    let result = party
        .get_partial_signatures(&NonceSharesMessage::default())
        .await;
    assert_refused(result, Code::NotFound, &message);
    assert_refused(party.start_payment().await, Code::NotFound, &message);
    let result = party
        .sign_deposit_tx(&PartialSignaturesMessage::default())
        .await;
    assert_refused(result, Code::NotFound, &message);
    let result = party.publish_deposit_tx(&DepositPsbt::default()).await;
    assert_refused(result, Code::NotFound, &message);
    let result = party.subscribe_tx_confirmation_status().await;
    assert_refused(result, Code::NotFound, &message);
    assert_refused(
        party.sign_swap_tx(&random_scalar()).await,
        Code::NotFound,
        &message,
    );
    assert_refused(party.release_payout().await, Code::NotFound, &message);
    let result = party.close_trade_with_key_share(&random_scalar()).await;
    assert_refused(result, Code::NotFound, &message);
    let result = party
        .sign_custom_payout_tx(SELLERS_CUSTOM_PAYOUT_AMOUNT)
        .await;
    assert_refused(result, Code::NotFound, &message);
    assert_refused(
        party.custom_close_trade(&unrelated_psbt()).await,
        Code::NotFound,
        &message,
    );

    assert!(!party.trades.contains_trade(&trade_id));
}

/// K2, second part: a second `InitTrade` with the ID of an open trade is rejected, and the trade
/// continues with the key shares of the first call.
#[tokio::test]
async fn k2_duplicate_init_trade() {
    for trade_type in TRADE_TYPES {
        // Directly after the first call.
        let mut trade = Trade::new(trade_type).await;
        trade.init_trades().await;
        for side in SIDES {
            let result = trade.party_mut(side).init_trade().await;
            assert_refused(result, Code::AlreadyExists, "already exists");
        }
        // The setup needs the key shares of the first calls, so it proves that they are kept.
        trade.exchange_nonce_shares().await;
        trade.exchange_partial_signatures().await;
        trade.sign_and_publish_deposit_tx().await;

        // After the deposit is signed, also with the other role.
        for side in SIDES {
            let party = trade.party_mut(side);
            assert_refused(
                party.init_trade().await,
                Code::AlreadyExists,
                "already exists",
            );
            let request = PubKeySharesRequest {
                trade_id: party.trade_id.clone(),
                my_role: if party.is_buyer() {
                    Role::SellerAsMaker
                } else {
                    Role::BuyerAsMaker
                }
                .into(),
            };
            assert_refused(
                party.client.init_trade(request).await,
                Code::AlreadyExists,
                "trade",
            );
        }
        trade.assert_states(TradeState::Deposit, TradeState::Deposit);
        trade.buyer_starts_payment().await;
        trade.seller_receives_payment_start().await;
        trade.release_payout().await;
        trade.buyer_closes_cooperatively().await;
        trade.seller_closes_cooperatively().await;
    }
}

/// K2, third part: `InitTrade` with the ID of a closed trade is rejected (N6).
#[tokio::test]
async fn k2_init_trade_with_id_of_closed_trade() {
    for stage in [
        Stage::ClosedCooperatively,
        Stage::ClosedByForce,
        Stage::ClosedByCustomPayout,
    ] {
        let mut trade = Trade::at(TradeType::TakerIsBuyer, stage).await;
        for side in SIDES {
            let party = trade.party_mut(side);
            let state = party.state();
            assert_refused(
                party.init_trade().await,
                Code::AlreadyExists,
                "already exists",
            );
            assert_eq!(party.state(), state, "{stage:?}");
        }
    }
}

/// K3: the two variants of `SignSwapTx` are refused for the buyer in every stage.
#[tokio::test]
async fn k3_seller_only_calls_by_buyer() {
    for trade_type in TRADE_TYPES {
        for stage in STAGES {
            let mut trade = Trade::at(trade_type, stage).await;
            // The signature that the seller's service would accept, if the buyer created it.
            let signature = trade
                .payment_started
                .as_ref()
                .and_then(|message| message.swap_tx_input_partial_signature.clone())
                .unwrap_or_else(random_scalar);
            let buyer = &mut trade.buyer;
            let state = buyer.state();

            let message = "operation only available for seller";
            let result = buyer.sign_swap_tx(&signature).await;
            assert_refused(result, Code::FailedPrecondition, message);
            let result = buyer.release_payout().await;
            assert_refused(result, Code::FailedPrecondition, message);
            assert_eq!(buyer.state(), state, "{stage:?}");
        }
    }
}

/// K3, the reverse: the seller cannot close the trade with a swap transaction, as the buyer does.
#[tokio::test]
async fn k3_buyer_only_close_by_seller() {
    for trade_type in TRADE_TYPES {
        for stage in [Stage::SellerReleased, Stage::SellerClosedByForce] {
            let mut trade = Trade::at(trade_type, stage).await;
            let swap_tx = match stage {
                Stage::SellerClosedByForce => trade.swap_tx(),
                _ => unrelated_tx(),
            };
            let state = trade.seller.state();
            let result = trade.seller.close_trade_with_swap_tx(&swap_tx).await;
            assert_refused(
                result,
                Code::FailedPrecondition,
                "operation only available for buyer",
            );
            assert_eq!(trade.seller.state(), state, "{stage:?}");
        }
    }
}

/// K3, the reverse: the second call of `GetPartialSignatures`, by which the buyer reports the
/// started payment, is refused for the seller in every stage.
#[tokio::test]
async fn k3_buyer_only_payment_start_by_seller() {
    for trade_type in TRADE_TYPES {
        for stage in STAGES {
            let mut trade = Trade::at(trade_type, stage).await;
            let state = trade.seller.state();
            let result = trade.seller.start_payment().await;
            assert_refused(
                result,
                Code::FailedPrecondition,
                "operation only available for buyer",
            );
            assert_eq!(trade.seller.state(), state, "{stage:?}");
        }
    }
}
