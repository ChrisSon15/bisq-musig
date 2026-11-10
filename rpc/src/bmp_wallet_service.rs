//! The user-facing wallet service, backed by [`BMPWallet`].
//!
//! Layering mirrors [`crate::wallet`]/[`crate::server`]:
//!
//! * [`BMPWalletServiceImpl`] owns the wallet — once the `OpenOrCreateWallet` RPC has opened or
//!   created it — and forwards every operation to [`BMPWallet`]. It implements
//!   [`BmpWalletService`], the GUI-oriented operations bisq2 needs, which deliberately does not
//!   depend on the bitcoind-oriented [`crate::wallet::WalletService`] trait.
//! * [`BmpWalletImpl`] is the thin gRPC adapter over `BmpWalletService`, serving the
//!   `wallet.Wallet` service defined in `bmp_wallet.proto` — the contract derived from bisq2's own
//!   `wallet.proto`, with the divergences from it listed at the top of that file.
//!
//! Unlike `WalletServiceImpl`, which drives a `bdk_bitcoind_rpc` emitter, this service syncs by
//! periodically calling [`WalletApi::sync_all`] against a [`ChainDataSource`] (compact block
//! filters by default), which is how `BMPWallet` is designed to see the chain.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// Leading `::` disambiguates the `wallet` *crate* from this crate's own `wallet` module.
use ::wallet::bmp_wallet::{BMPWallet, WalletApi as _};
use ::wallet::chain_data_source::ChainDataSource;
use ::wallet::password::{InvalidPassword, Password, WeakPassword};
use ::wallet::error::WalletErrorKind;
use ::wallet::wallet_info::{PubAddressInfo, TxInfo, TxOutputInfo, UtxoInfo};
use bdk_bitcoind_rpc::bitcoincore_rpc::{Client, RpcApi as _};
use bdk_wallet::Balance;
use bdk_wallet::bitcoin::address::{NetworkUnchecked, ParseError};
use bdk_wallet::bitcoin::{Address, Amount, FeeRate, Network, Transaction, Txid};
use chain::{ChainApi, ChainApiError};
use futures_util::never::Never;
use futures_util::stream::{BoxStream, StreamExt as _};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::{self, JoinHandle};
use tokio::time::{self, Duration, MissedTickBehavior};
use tonic::{Request, Response, Result, Status};
use tracing::{debug, error, info, instrument};
use zeroize::Zeroizing;

use crate::observable::ObservableHashMap;
pub use crate::pb::bmp_wallet::wallet_server::WalletServer as BmpWalletServer;
use crate::pb::bmp_wallet::{
    self, ChangePasswordRequest, ChangePasswordResponse, GetBalanceRequest, GetBalanceResponse,
    GetSeedWordsRequest, GetSeedWordsResponse, GetUnusedAddressRequest, GetUnusedAddressResponse,
    GetWalletAddressesRequest, GetWalletAddressesResponse, IsWalletReadyRequest,
    IsWalletReadyResponse, ListTransactionsRequest, ListTransactionsResponse, ListUtxosRequest,
    ListUtxosResponse, OpenOrCreateWalletRequest, OpenOrCreateWalletResponse, SendToAddressRequest,
    SendToAddressResponse, wallet_server,
};
use crate::server::handle_request_async;
use crate::wallet::{Result as WalletResult, TxConfidence, tx_confidence_entries};

/// How often the wallet is re-synced against the chain data source.
const DEFAULT_POLL_PERIOD: Duration = Duration::from_secs(30);

/// How often the sync loop re-checks whether the wallet has been opened yet (the wallet is
/// opened over RPC, so the loop may start before there is anything to sync).
const WALLET_OPEN_POLL_PERIOD: Duration = Duration::from_millis(500);

/// Fee rate used when the client doesn't specify one. 2 sat/vB == 500 sat/kwu.
const DEFAULT_FEE_RATE: FeeRate = FeeRate::from_sat_per_kwu(500);

/// A wallet operation was attempted before `OpenOrCreateWallet` opened the wallet. Reported to
/// gRPC clients as `FAILED_PRECONDITION`.
#[derive(Debug, Error)]
#[error("wallet is not open; call OpenOrCreateWallet first")]
pub struct WalletNotOpen;

/// The GUI-facing wallet operations.
///
/// Every method maps one-to-one onto an RPC of the `wallet.Wallet` service and, in turn, onto a
/// method of [`BMPWallet`]. Kept as a trait (rather than inherent methods) so the gRPC adapter
/// can hold `Arc<dyn BmpWalletService>` and be exercised against a stub in tests. Deliberately
/// *not* a subtrait of [`crate::wallet::WalletService`]: that trait models the bitcoind-backed
/// trade wallet, which this user-facing wallet no longer depends on.
#[expect(
    clippy::double_must_use,
    reason = "tonic::async_trait marks generated futures as must-use"
)]
#[tonic::async_trait]
pub trait BmpWalletService {
    /// Whether the wallet is open and, if a chain data source is configured, has completed at
    /// least one sync.
    fn is_ready(&self) -> bool;

    /// Opens the wallet database, creating a fresh wallet only if none exists yet.
    ///
    /// `password` protects the database. When a wallet is created it becomes the wallet's
    /// password and so must follow the password rules (see [`Password`]), failing with
    /// [`WeakPassword`] otherwise. When an existing wallet is opened, or an already-open one is
    /// re-opened, a wrong password fails with [`InvalidPassword`]. Every other wallet operation
    /// requires this to have been called first.
    async fn open_or_create_wallet(&self, password: &str) -> anyhow::Result<()>;

    /// Re-keys the wallet from `old_password` to `new_password`.
    ///
    /// `old_password` must be the password currently in force ([`InvalidPassword`] otherwise),
    /// and `new_password` must follow the password rules ([`WeakPassword`] otherwise). The
    /// wallet is always password-protected, so a password can only be replaced, never removed.
    async fn change_password(&self, old_password: &str, new_password: &str) -> anyhow::Result<()>;

    /// Hands out an unused receive address.
    ///
    /// This routes through the gap-capped [`BMPWallet::next_address`], so a click-happy client
    /// cycles through the existing unused addresses once the gap limit is reached instead of
    /// growing it without bound. Either way a different address is returned on every call.
    async fn unused_address(&self) -> anyhow::Result<PubAddressInfo>;

    async fn wallet_addresses(&self) -> anyhow::Result<Vec<PubAddressInfo>>;

    async fn transactions(&self) -> anyhow::Result<Vec<TxInfo>>;

    async fn utxos(&self) -> anyhow::Result<Vec<UtxoInfo>>;

    /// Builds, signs, persists and broadcasts a payment.
    ///
    /// `password` must be the wallet password; a wrong one fails with [`InvalidPassword`].
    /// `address` is tied to the wallet's network here, which is why it arrives unchecked.
    async fn send_to_address(
        &self,
        password: &str,
        address: Address<NetworkUnchecked>,
        amount: Amount,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<Txid>;

    async fn full_balance(&self) -> anyhow::Result<Balance>;

    /// The wallet's seed phrase, one word per element.
    ///
    /// `password` must be the wallet password; a wrong one fails with [`InvalidPassword`] rather
    /// than revealing anything. A stored seed that is not a valid BIP39 mnemonic also fails.
    async fn seed_words(&self, password: &str) -> anyhow::Result<Vec<String>>;
}

/// A [`BmpWalletService`] whose every operation is forwarded to [`BMPWallet`].
///
/// The wallet itself starts out absent: it is opened (or created) in the configured directory by
/// [`BmpWalletService::open_or_create_wallet`], driven by the `OpenOrCreateWallet` RPC, so the
/// database password never has to reach the server's command line.
///
/// `S` is the chain backend used for syncing. It is a type parameter rather than a trait object
/// because [`ChainDataSource::sync`] is generic over the persister and so isn't object safe;
/// callers that don't want syncing at all can leave it unset (see [`Self::new`]).
pub struct BMPWalletServiceImpl<S> {
    /// Async mutex, because [`WalletApi::sync_all`] is an `async fn` taking `&mut self` and so
    /// must be awaited while the lock is held. `None` until `OpenOrCreateWallet` has been
    /// called.
    ///
    /// NOTE: to avoid deadlocks, acquire this before `tx_confidence_map`, never the other way
    /// round — the same ordering `WalletServiceImpl` uses.
    wallet: AsyncMutex<Option<BMPWallet>>,
    /// Directory holding (or to hold) the wallet database.
    wallet_dir: PathBuf,
    network: Network,
    tx_confidence_map: Mutex<ObservableHashMap<Txid, TxConfidence>>,
    chain_data_source: Option<S>,
    broadcaster: Option<Arc<dyn ChainApi>>,
    ready: AtomicBool,
    poll_period: Duration,
}

/// The open wallet behind `guard`, or [`WalletNotOpen`] if `OpenOrCreateWallet` hasn't
/// succeeded yet.
fn require_open(guard: &mut Option<BMPWallet>) -> anyhow::Result<&mut BMPWallet> {
    guard
        .as_mut()
        .ok_or_else(|| anyhow::Error::new(WalletNotOpen))
}

impl<S> BMPWalletServiceImpl<S> {
    /// Creates the service with no wallet open yet; `OpenOrCreateWallet` supplies the password
    /// and opens (or creates) the wallet in `wallet_dir`. Without a chain data source the
    /// wallet never syncs: it still serves addresses, seed words and password changes, but will
    /// not see any funds.
    pub fn new(wallet_dir: impl Into<PathBuf>, network: Network) -> Self {
        Self {
            wallet: AsyncMutex::new(None),
            wallet_dir: wallet_dir.into(),
            network,
            tx_confidence_map: Mutex::new(ObservableHashMap::new()),
            chain_data_source: None,
            broadcaster: None,
            ready: AtomicBool::new(false),
            poll_period: DEFAULT_POLL_PERIOD,
        }
    }

    #[must_use]
    pub fn with_chain_data_source(self, chain_data_source: S) -> Self {
        Self {
            chain_data_source: Some(chain_data_source),
            ..self
        }
    }

    /// Supplies the backend used to publish transactions built by
    /// [`BmpWalletService::send_to_address`]. Without one, `SendToAddress` fails rather than
    /// silently returning an unbroadcast txid.
    #[must_use]
    pub fn with_broadcaster(self, broadcaster: Arc<dyn ChainApi>) -> Self {
        Self {
            broadcaster: Some(broadcaster),
            ..self
        }
    }

    #[must_use]
    pub fn with_poll_period(self, poll_period: Duration) -> Self {
        Self {
            poll_period,
            ..self
        }
    }

    pub fn get_tx_confidence_stream(&self, txid: Txid) -> BoxStream<'static, Option<TxConfidence>> {
        self.tx_confidence_map.lock().unwrap().observe(txid).boxed()
    }
}

impl<S: ChainDataSource + Sync> BMPWalletServiceImpl<S> {
    /// Syncs the wallet once, returning whether there was an open wallet to sync at all.
    async fn sync_once(&self, chain_data_source: &S) -> anyhow::Result<bool> {
        let mut guard = self.wallet.lock().await;
        let Some(wallet) = guard.as_mut() else {
            return Ok(false);
        };
        wallet.sync_all(chain_data_source).await?;

        // TODO: Skip needless map updates if the wallet hasn't actually changed.
        self.tx_confidence_map
            .lock()
            .unwrap()
            .sync(tx_confidence_entries(wallet));
        Ok(true)
    }
}

impl<S: ChainDataSource + Send + Sync + 'static> BMPWalletServiceImpl<S> {
    /// Periodically re-syncs the wallet through its [`ChainDataSource`], mirroring what
    /// [`crate::wallet::WalletService::connect`] does for the bitcoind-backed wallet without
    /// this type having to implement that trait.
    ///
    /// The `rpc` argument is used only to log which node we're pointed at — `BMPWallet` reaches
    /// the chain over compact block filters, not Bitcoin Core RPC, so it plays no part in
    /// syncing here.
    ///
    /// The wallet itself is opened by the `OpenOrCreateWallet` RPC, so syncing (and with it the
    /// ready flag) waits until that has happened.
    ///
    /// # Errors
    /// Will return `Err` if the initial or any subsequent sync fails.
    pub async fn connect(&self, rpc: Arc<Client>) -> WalletResult<Never> {
        match task::block_in_place(|| rpc.get_blockchain_info()) {
            Ok(info) => info!(chain = %info.chain, blocks = info.blocks,
                "Bitcoin Core reachable (informational only; syncing uses the chain data source)."),
            Err(e) => debug!("Bitcoin Core RPC not reachable ({e}); continuing regardless."),
        }

        let Some(chain_data_source) = self.chain_data_source.as_ref() else {
            info!("No chain data source configured; wallet will not sync.");
            // `open_or_create_wallet` flips the ready flag as soon as the wallet is open, since
            // everything that doesn't depend on chain state is available immediately.
            return Ok(std::future::pending().await);
        };

        info!("Waiting for the wallet to be opened, to perform the initial sync...");
        while !self.sync_once(chain_data_source).await? {
            time::sleep(WALLET_OPEN_POLL_PERIOD).await;
        }
        self.ready.store(true, Ordering::Release);
        let total = self
            .wallet
            .lock()
            .await
            .as_ref()
            .map(|wallet| wallet.full_balance().total())
            .unwrap_or_default();
        info!(wallet_balance_total = %total, "Finished initial sync.");

        let mut interval = time::interval(self.poll_period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            interval.tick().await;
            self.sync_once(chain_data_source).await?;
        }
    }

    /// Spawns [`Self::connect`] onto the Tokio runtime, mirroring
    /// [`crate::wallet::WalletService::spawn_connection`].
    ///
    /// # Panics
    /// Will panic if called outside the context of a Tokio runtime.
    pub fn spawn_connection(
        self: Arc<Self>,
        client: Arc<Client>,
    ) -> JoinHandle<WalletResult<Never>> {
        task::spawn(async move {
            self.connect(client)
                .await
                .inspect_err(|e| error!("Wallet connection error: {e}"))
        })
    }
}

#[tonic::async_trait]
impl<S: ChainDataSource + Send + Sync + 'static> BmpWalletService for BMPWalletServiceImpl<S> {
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    async fn open_or_create_wallet(&self, password: &str) -> anyhow::Result<()> {
        let mut guard = self.wallet.lock().await;

        if let Some(wallet) = guard.as_ref() {
            // Idempotent for a client that reconnects: re-opening an open wallet succeeds, but
            // only for a caller who can present the password currently in force.
            if wallet.check_password(&Password::check(password)?)? {
                return Ok(());
            }
            return Err(InvalidPassword.into());
        }

        // Creating is chosen on the *absence* of the database file, never on a failed load. A
        // load failure most likely means a wrong password was given, and creating a wallet
        // rewrites the Argon2 salt the existing database's key was derived from — which would
        // render that wallet, and any funds in it, permanently unrecoverable.
        fs::create_dir_all(&self.wallet_dir)?;
        let db_path = self.wallet_dir.join(BMPWallet::DB_NAME);
        let wallet = if db_path.exists() {
            let wallet = match BMPWallet::load_wallet(
                self.wallet_dir.as_path().into(),
                self.network,
                &Password::check(password)?,
            ) {
                Ok(wallet) => wallet,
                Err(err) => match err {
                    WalletErrorKind::InvalidPassword => {
                        return Err(anyhow::Error::new(InvalidPassword).context(format!(
                            "failed to open the existing wallet at {} (wrong password)",
                            db_path.display()
                        )));
                    }
                    other => {
                        return Err(anyhow::Error::new(other).context(format!(
                            "failed to open the existing wallet at {}",
                            db_path.display()
                        )));
                    }
                },
            };
            info!(dir = %self.wallet_dir.display(), "Loaded the existing BMP wallet.");
            wallet
        } else {
            // Only now does the password become *the* wallet password, so only now does breaking
            // the password rules make it a bad argument rather than a wrong password (compare
            // `Password::check`).
            let password = Password::new(password)?;
            info!(dir = %self.wallet_dir.display(), "No BMP wallet found; creating a new one.");
            BMPWallet::new(self.wallet_dir.as_path().into(), &password, self.network)?
        };

        self.tx_confidence_map
            .lock()
            .unwrap()
            .sync(tx_confidence_entries(&wallet));
        *guard = Some(wallet);

        // With no chain data source there is nothing to sync, so the wallet is ready as soon as
        // it is open; otherwise the sync loop flips the flag after the first successful sync.
        if self.chain_data_source.is_none() {
            self.ready.store(true, Ordering::Release);
        }
        Ok(())
    }

    async fn change_password(&self, old_password: &str, new_password: &str) -> anyhow::Result<()> {
        let mut guard = self.wallet.lock().await;
        let wallet = require_open(&mut guard)?;
        // The wallet verifies the old password itself, one Argon2 derivation under the lock, so
        // checking it here too would only double that cost.
        let old_password = Password::check(old_password)?;
        let new_password = Password::new(new_password)?;

        wallet
        
            .change_password(&old_password, &new_password)
            .map_err(|err| match err {
                WalletErrorKind::InvalidPassword => InvalidPassword.into(),
                other => anyhow::Error::new(other),
            })
    }

    async fn unused_address(&self) -> anyhow::Result<PubAddressInfo> {
        let mut guard = self.wallet.lock().await;
        let address = require_open(&mut guard)?.get_new_address()?.address;
        Ok(PubAddressInfo { address })
    }

    async fn wallet_addresses(&self) -> anyhow::Result<Vec<PubAddressInfo>> {
        let mut guard = self.wallet.lock().await;
        Ok(require_open(&mut guard)?.list_wallet_addresses())
    }

    async fn transactions(&self) -> anyhow::Result<Vec<TxInfo>> {
        let mut guard = self.wallet.lock().await;
        Ok(require_open(&mut guard)?.list_transactions())
    }

    async fn utxos(&self) -> anyhow::Result<Vec<UtxoInfo>> {
        let mut guard = self.wallet.lock().await;
        Ok(require_open(&mut guard)?.list_utxos())
    }

    async fn send_to_address(
        &self,
        password: &str,
        address: Address<NetworkUnchecked>,
        amount: Amount,
        fee_rate: Option<FeeRate>,
    ) -> anyhow::Result<Txid> {
        let broadcaster = self.broadcaster.as_ref().ok_or_else(|| {
            anyhow::anyhow!("no chain backend configured for broadcasting transactions")
        })?;

        let mut guard = self.wallet.lock().await;
        let wallet = require_open(&mut guard)?;

        if !wallet.check_password(&Password::check(password)?)? {
            return Err(InvalidPassword.into());
        }

        // Only now is the wallet's network known, so this is where the address gets tied to it.
        let address = address.require_network(wallet.network())?;

        let tx: Transaction =
            wallet.send_to_address(&address, amount, fee_rate.unwrap_or(DEFAULT_FEE_RATE))?;

        // Broadcast while still holding the lock, so a concurrent send can't build a conflicting
        // transaction from the same UTXO set before this one is out. `transaction_broadcast` is
        // a blocking network call, so it must not run directly on an async worker thread.
        let txid = task::block_in_place(|| broadcaster.transaction_broadcast(&tx))?;
        info!(%txid, "Broadcast wallet payment.");
        Ok(txid)
    }

    async fn full_balance(&self) -> anyhow::Result<Balance> {
        let mut guard = self.wallet.lock().await;
        Ok(require_open(&mut guard)?.full_balance())
    }

    async fn seed_words(&self, password: &str) -> anyhow::Result<Vec<String>> {
        let mut guard = self.wallet.lock().await;
        let wallet = require_open(&mut guard)?;

        if !wallet.check_password(&Password::check(password)?)? {
            return Err(InvalidPassword.into());
        }

        let mnemonic = wallet.get_seed_phrase()?;
        Ok(mnemonic.words().map(ToOwned::to_owned).collect())
    }
}

/// Publishes transactions through a Bitcoin Core node.
pub struct BitcoinCoreChainApi(Arc<Client>);

impl BitcoinCoreChainApi {
    pub const fn new(client: Arc<Client>) -> Self {
        Self(client)
    }
}

impl ChainApi for BitcoinCoreChainApi {
    fn transaction_broadcast(&self, tx: &Transaction) -> std::result::Result<Txid, ChainApiError> {
        self.0
            .send_raw_transaction(tx)
            .map_err(ChainApiError::backend)
    }
}

/// gRPC adapter serving `wallet.Wallet`, the contract bisq2's `WalletGrpcClient` speaks.
///
/// A password arrives as a plain `String` in the decoded request. Each adapter moves it into a
/// [`Zeroizing`] buffer before lending it to the service, so that copy is wiped on drop just
/// like the [`Password`] the service builds from it, instead of lingering in freed heap memory.
pub struct BmpWalletImpl {
    pub wallet_service: Arc<dyn BmpWalletService + Send + Sync>,
}

#[tonic::async_trait]
impl wallet_server::Wallet for BmpWalletImpl {
    #[instrument(skip_all)]
    async fn open_or_create_wallet(
        &self,
        request: Request<OpenOrCreateWalletRequest>,
    ) -> Result<Response<OpenOrCreateWalletResponse>> {
        handle_request_async(request, |request| async move {
            let password = Zeroizing::new(request.password);
            self.wallet_service
                .open_or_create_wallet(&password)
                .await
                .map_err(|e| status_from(&e))?;

            Ok(OpenOrCreateWalletResponse { success: true })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn is_wallet_ready(
        &self,
        request: Request<IsWalletReadyRequest>,
    ) -> Result<Response<IsWalletReadyResponse>> {
        handle_request_async(request, |_request| async {
            Ok(IsWalletReadyResponse {
                ready: self.wallet_service.is_ready(),
            })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn get_unused_address(
        &self,
        request: Request<GetUnusedAddressRequest>,
    ) -> Result<Response<GetUnusedAddressResponse>> {
        handle_request_async(request, |_request| async {
            let address = self
                .wallet_service
                .unused_address()
                .await
                .map_err(|e| status_from(&e))?;

            Ok(GetUnusedAddressResponse {
                address: Some(address.into()),
            })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn get_wallet_addresses(
        &self,
        request: Request<GetWalletAddressesRequest>,
    ) -> Result<Response<GetWalletAddressesResponse>> {
        handle_request_async(request, |_request| async {
            let addresses = self
                .wallet_service
                .wallet_addresses()
                .await
                .map_err(|e| status_from(&e))?
                .into_iter()
                .map(Into::into)
                .collect();

            Ok(GetWalletAddressesResponse { addresses })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn list_transactions(
        &self,
        request: Request<ListTransactionsRequest>,
    ) -> Result<Response<ListTransactionsResponse>> {
        handle_request_async(request, |_request| async {
            let transactions = self
                .wallet_service
                .transactions()
                .await
                .map_err(|e| status_from(&e))?
                .into_iter()
                .map(Into::into)
                .collect();

            Ok(ListTransactionsResponse { transactions })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn list_utxos(
        &self,
        request: Request<ListUtxosRequest>,
    ) -> Result<Response<ListUtxosResponse>> {
        handle_request_async(request, |_request| async {
            let utxos = self
                .wallet_service
                .utxos()
                .await
                .map_err(|e| status_from(&e))?
                .into_iter()
                .map(Into::into)
                .collect();

            Ok(ListUtxosResponse { utxos })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn send_to_address(
        &self,
        request: Request<SendToAddressRequest>,
    ) -> Result<Response<SendToAddressResponse>> {
        handle_request_async(request, |request| async move {
            let address = request
                .address
                .ok_or_else(|| Status::invalid_argument("address is required"))?;
            let address = Address::<NetworkUnchecked>::try_from(address)
                .map_err(|e| status_from(&e.into()))?;

            let passphrase = Zeroizing::new(request.passphrase);
            let tx_id = self
                .wallet_service
                .send_to_address(
                    &passphrase,
                    address,
                    Amount::from_sat(request.amount),
                    request.fee_rate_per_kwu.map(FeeRate::from_sat_per_kwu),
                )
                .await
                .map_err(|e| status_from(&e))?;

            Ok(SendToAddressResponse {
                tx_id: tx_id.to_string(),
            })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn get_balance(
        &self,
        request: Request<GetBalanceRequest>,
    ) -> Result<Response<GetBalanceResponse>> {
        handle_request_async(request, |_request| async {
            let balance = self
                .wallet_service
                .full_balance()
                .await
                .map_err(|e| status_from(&e))?;

            Ok(balance.into())
        })
        .await
    }

    #[instrument(skip_all)]
    async fn get_seed_words(
        &self,
        request: Request<GetSeedWordsRequest>,
    ) -> Result<Response<GetSeedWordsResponse>> {
        handle_request_async(request, |request| async move {
            let password = Zeroizing::new(request.password);
            let seed_words = self
                .wallet_service
                .seed_words(&password)
                .await
                .map_err(|e| status_from(&e))?;

            Ok(GetSeedWordsResponse { seed_words })
        })
        .await
    }

    #[instrument(skip_all)]
    async fn change_password(
        &self,
        request: Request<ChangePasswordRequest>,
    ) -> Result<Response<ChangePasswordResponse>> {
        handle_request_async(request, |request| async move {
            let old_password = Zeroizing::new(request.old_password);
            let new_password = Zeroizing::new(request.new_password);
            self.wallet_service
                .change_password(&old_password, &new_password)
                .await
                .map_err(|e| status_from(&e))?;

            Ok(ChangePasswordResponse { success: true })
        })
        .await
    }
}

/// Maps a wallet-layer error onto the gRPC status codes a client can distinguish: using the
/// wallet before opening it is a state error the caller can fix (`FAILED_PRECONDITION`), a
/// wrong password is the caller's mistake (`PERMISSION_DENIED`), a new password breaking the
/// password rules is a bad argument whose message tells the user how to fix it
/// (`INVALID_ARGUMENT`), and everything else is a server fault (`INTERNAL`).
fn status_from(error: &anyhow::Error) -> Status {
    // The alternate form includes the causes, e.g. "failed to open the existing wallet at …:
    // invalid wallet password", where `to_string()` would stop at the context.
    let message = format!("{error:#}");
    if error.downcast_ref::<WalletNotOpen>().is_some() {
        Status::failed_precondition(message)
    } else if is_invalid_password(error) {
        Status::permission_denied(message)
    } else if error.downcast_ref::<WeakPassword>().is_some() {
        // The message is the password rules in plain English, meant for the user's eyes.
        Status::invalid_argument(message)
    } else if error.downcast_ref::<ParseError>().is_some() {
        // A malformed address, or one for another network: the client's fault, not ours.
        Status::invalid_argument(format!("invalid address: {message}"))
    } else if matches!(
        error.downcast_ref::<WalletErrorKind>(),
        Some(WalletErrorKind::WalletAlreadyExists)
    ) {
        Status::already_exists(message)
    } else {
        error!("Wallet operation failed: {message}");
        Status::internal(message)
    }
}
/// Whether `error` is a wrong wallet password, whichever layer caught it: the service, checking
/// a password itself ([`InvalidPassword`]), or the wallet, when a load or re-key was refused
/// ([`WalletErrorKind::InvalidPassword`], which wraps the same error).
fn is_invalid_password(error: &anyhow::Error) -> bool {
    error.downcast_ref::<InvalidPassword>().is_some()
        || matches!(
            error.downcast_ref::<WalletErrorKind>(),
            Some(WalletErrorKind::InvalidPassword)
        )
}

// Conversions from the `wallet` crate's transport-agnostic views to the generated protobuf types.

impl From<Balance> for GetBalanceResponse {
    fn from(value: Balance) -> Self {
        Self {
            // bisq2 reads only `balance`; keep it the trusted-spendable total it expects.
            balance: value.trusted_spendable().to_sat(),
            confirmed: value.confirmed.to_sat(),
            trusted_pending: value.trusted_pending.to_sat(),
            untrusted_pending: value.untrusted_pending.to_sat(),
            immature: value.immature.to_sat(),
        }
    }
}

impl From<PubAddressInfo> for bmp_wallet::PubAddressInfo {
    fn from(value: PubAddressInfo) -> Self {
        Self {
            address: value.address.to_string(),
        }
    }
}

impl TryFrom<bmp_wallet::PubAddressInfo> for Address<NetworkUnchecked> {
    type Error = ParseError;

    /// Parses the address without checking its network: the transport layer doesn't know which
    /// one the wallet is on, so the result stays unchecked until
    /// [`BmpWalletService::send_to_address`] ties it to the open wallet.
    fn try_from(value: bmp_wallet::PubAddressInfo) -> std::result::Result<Self, Self::Error> {
        value.address.parse()
    }
}

impl From<UtxoInfo> for bmp_wallet::Utxo {
    fn from(value: UtxoInfo) -> Self {
        Self {
            tx_id: value.tx_id.to_string(),
            vout: value.vout,
            amount: value.amount.to_sat(),
            address: value.address.map(Into::into),
            num_confirmations: value.num_confirmations,
        }
    }
}

impl From<TxOutputInfo> for bmp_wallet::TransactionOutput {
    fn from(value: TxOutputInfo) -> Self {
        Self {
            value: value.value.to_sat(),
            address: value.address.map(Into::into),
            script_pub_key: value.script_pubkey.into_bytes(),
        }
    }
}

impl From<TxInfo> for bmp_wallet::Transaction {
    fn from(value: TxInfo) -> Self {
        Self {
            tx_id: value.tx_id.to_string(),
            inputs: value
                .inputs
                .into_iter()
                .map(|input| bmp_wallet::TransactionInput {
                    prev_out_tx_id: input.prev_out_tx_id.to_string(),
                    // Proto uses int32/int64 here (bisq2's choice, to match Java's signed
                    // types), so widen rather than truncate.
                    prev_out_index: i32::try_from(input.prev_out_index).unwrap_or(-1),
                    sequence_number: i64::from(input.sequence),
                    script_sig: input.script_sig.into_bytes(),
                    witness: input.witness,
                })
                .collect(),
            outputs: value.outputs.into_iter().map(Into::into).collect(),
            lock_time: u64::from(value.lock_time),
            block_height: value.block_height.unwrap_or_default(),
            // Seconds since the epoch, which is how bisq2's `Transaction.fromProto` decodes it.
            date: value.timestamp.unwrap_or_default(),
            num_confirmations: value.num_confirmations,
            amount: value.amount.to_sat(),
            incoming: value.incoming,
        }
    }
}

#[cfg(test)]
mod tests {
    use ::wallet::error::ChainDataSourceError;
    use ::wallet::persisted::{BMPWalletPersister as _, DBStorage};
    use ::wallet::utils::derive_key_from_password;
    use bdk_wallet::PersistedWallet;
    use bdk_wallet::bitcoin::Network;
    use bdk_wallet::rusqlite::Connection;
    use tempfile::tempdir;

    use super::*;

    /// A `ChainDataSource` that does nothing, so tests can exercise the service without a chain.
    struct NoopChainDataSource;

    impl ChainDataSource for NoopChainDataSource {
        const RECOVERY_HEIGHT: usize = 0;
        const BATCH_SIZE: usize = 1;
        const STOP_GAP: usize = 1;

        #[expect(
            clippy::unused_async_trait_impl,
            reason = "the ChainDataSource trait requires an async method"
        )]
        async fn sync(
            &self,
            _persister: Vec<&mut PersistedWallet<impl wallet::persisted::BMPWalletPersister>>,
        ) -> Result<(), ChainDataSourceError> {
            Ok(())
        }
    }

    /// A broadcaster for tests that must fail before anything is broadcast.
    struct UnreachableBroadcaster;

    impl ChainApi for UnreachableBroadcaster {
        fn transaction_broadcast(&self, _tx: &Transaction) -> Result<Txid, ChainApiError> {
            Err(ChainApiError::backend(std::io::Error::other(
                "nothing should have been broadcast",
            )))
        }
    }

    const REGTEST_ADDRESS: &str = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";
    const MAINNET_ADDRESS: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";
    /// Passwords following the wallet's password rules.
    const PASSWORD: &str = "S3cret!!";
    const NEW_PASSWORD: &str = "Hunter#2";

    /// A regtest address as the wallet itself would hand it out.
    fn address_info(address: &str) -> PubAddressInfo {
        let address = address
            .parse::<Address<NetworkUnchecked>>()
            .unwrap()
            .require_network(Network::Regtest)
            .unwrap();
        PubAddressInfo { address }
    }

    fn service() -> (tempfile::TempDir, BMPWalletServiceImpl<NoopChainDataSource>) {
        let dir = tempdir().unwrap();
        let service = BMPWalletServiceImpl::new(dir.path(), Network::Regtest);
        (dir, service)
    }

    async fn open_service() -> (tempfile::TempDir, BMPWalletServiceImpl<NoopChainDataSource>) {
        let (dir, service) = service();
        service.open_or_create_wallet(PASSWORD).await.unwrap();
        (dir, service)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn operations_before_open_are_refused() {
        let (_dir, service) = service();

        assert!(!service.is_ready(), "no wallet open, so not ready");

        let err = service.unused_address().await.unwrap_err();
        assert!(
            err.downcast_ref::<WalletNotOpen>().is_some(),
            "expected WalletNotOpen, got: {err}"
        );
        assert!(service.full_balance().await.is_err());
        assert!(service.seed_words(PASSWORD).await.is_err());
        assert!(service.transactions().await.is_err());
        assert!(service.utxos().await.is_err());
        assert!(service.wallet_addresses().await.is_err());
        assert!(
            service
                .change_password(PASSWORD, NEW_PASSWORD)
                .await
                .is_err()
        );    
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forwards_queries_to_the_wallet() {
        let (_dir, service) = open_service().await;

        assert!(
            service.is_ready(),
            "no chain data source, so the wallet is ready as soon as it is open"
        );
        assert_eq!(service.full_balance().await.unwrap().total().to_sat(), 0);
        assert_eq!(service.seed_words(PASSWORD).await.unwrap().len(), 24);
        assert_eq!(service.transactions().await.unwrap().len(), 0);
        assert_eq!(service.utxos().await.unwrap().len(), 0);

        // Revealing an address must show up in the address list.
        assert_eq!(service.wallet_addresses().await.unwrap().len(), 0);
        let address = service.unused_address().await.unwrap();
        assert!(service.wallet_addresses().await.unwrap().contains(&address));

        // Asking again hands out a fresh address, distinct from the one just handed out.
        let next_address = service.unused_address().await.unwrap();
        assert_ne!(next_address, address);
        assert!(
            service
                .wallet_addresses()
                .await
                .unwrap()
                .contains(&next_address)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_is_idempotent_but_checks_the_password() {
        let (dir, service) = service();

        service.open_or_create_wallet(PASSWORD).await.unwrap();
        let seed = service.seed_words(PASSWORD).await.unwrap();

        // The seed is only revealed to the holder of the password.
        let err = service.seed_words("wrong").await.unwrap_err();
        assert!(
            is_invalid_password(&err),
            "expected InvalidPassword, got: {err}"
        );

        // Re-opening with the right password is a no-op; a wrong one is rejected.
        service.open_or_create_wallet(PASSWORD).await.unwrap();
        let err = service.open_or_create_wallet("wrong").await.unwrap_err();
        assert!(
            is_invalid_password(&err),
            "expected InvalidPassword, got: {err}"
        );

        // A fresh service on the same directory must load the persisted wallet, not create a
        // new one — and only for the holder of the password. A wrong password that follows the
        // rules gets as far as the wallet, whose own refusal must come out the same way.
        let reloaded =
            BMPWalletServiceImpl::<NoopChainDataSource>::new(dir.path(), Network::Regtest);
        let err = reloaded
            .open_or_create_wallet(NEW_PASSWORD)
            .await
            .unwrap_err();
        assert!(
            is_invalid_password(&err),
            "a wrong password must not open (or overwrite!) the existing wallet, got: {err}"
        );
        assert_eq!(status_from(&err).code(), tonic::Code::PermissionDenied);
        reloaded.open_or_create_wallet(PASSWORD).await.unwrap();
        assert_eq!(reloaded.seed_words(PASSWORD).await.unwrap(), seed);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_recreates_a_wallet_when_only_its_seed_was_persisted() {
        let dir = tempdir().unwrap();
        let storage = DBStorage::File(dir.path().to_path_buf());
        let salt = storage.persist_salt(BMPWallet::DB_NAME).unwrap();
        let mut db = storage.open(BMPWallet::DB_NAME).unwrap();
        let key = derive_key_from_password(&PASSWORD.parse().unwrap(), &salt).unwrap();
        db.pragma_update(None, "key", key.as_str()).unwrap();

        Connection::init(
            &mut db,
            Some(BMPWallet::IMPORTED_KEYS_TABLE_NAME),
            Some(BMPWallet::SEEDS_TABLE_NAME),
        )
        .unwrap();
        let seed_phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        Connection::persist_seed_phrase(&mut db, BMPWallet::SEEDS_TABLE_NAME, seed_phrase).unwrap();
        drop(db);

        // The seed is durable, but wallet creation is simulated as having failed before BDK
        // could persist its wallet record. Reopening must recreate that wallet from the seed.
        let reopened =
            BMPWalletServiceImpl::<NoopChainDataSource>::new(dir.path(), Network::Regtest);
        reopened.open_or_create_wallet(PASSWORD).await.unwrap();
        assert!(reopened.is_ready());
        assert_eq!(
            reopened.seed_words(PASSWORD).await.unwrap().join(" "),
            seed_phrase
        );
        assert!(reopened.unused_address().await.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn seed_words_fails_for_a_corrupt_persisted_seed() {
        let dir = tempdir().unwrap();
        let created =
            BMPWalletServiceImpl::<NoopChainDataSource>::new(dir.path(), Network::Regtest);
        created.open_or_create_wallet(PASSWORD).await.unwrap();
        drop(created);

        let storage = DBStorage::File(dir.path().to_path_buf());
        let salt = storage.load_salt(BMPWallet::DB_NAME).unwrap();
        let mut db = storage.open(BMPWallet::DB_NAME).unwrap();
        let key = derive_key_from_password(&PASSWORD.parse().unwrap(), &salt).unwrap();
        db.pragma_update(None, "key", key.as_str()).unwrap();
        db.execute(&format!("DELETE FROM {}", BMPWallet::SEEDS_TABLE_NAME), [])
            .unwrap();
        Connection::persist_seed_phrase(
            &mut db,
            BMPWallet::SEEDS_TABLE_NAME,
            "abandon abandon abandon",
        )
        .unwrap();
        drop(db);

        let reopened =
            BMPWalletServiceImpl::<NoopChainDataSource>::new(dir.path(), Network::Regtest);
        reopened.open_or_create_wallet(PASSWORD).await.unwrap();

        let error = reopened.seed_words(PASSWORD).await.unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<WalletErrorKind>(),
                Some(WalletErrorKind::Mnemonic(_))
            ),
            "unexpected error: {error:#}"
        );
        assert!(!format!("{error:#}").contains("abandon"));
    }
    
    #[tokio::test(flavor = "multi_thread")]
    async fn creating_a_wallet_requires_an_acceptable_password() {
        let (dir, service) = service();

        for weak in ["", "short", "no digits or symbols"] {
            let err = service.open_or_create_wallet(weak).await.unwrap_err();
            assert!(
                err.downcast_ref::<WeakPassword>().is_some(),
                "{weak:?} must be rejected as a weak password, got: {err}"
            );
            assert_eq!(
                err.to_string(),
                Password::REQUIREMENTS,
                "the user must be told how to make an acceptable password"
            );
        }
        assert!(
            !dir.path().join(BMPWallet::DB_NAME).exists(),
            "a rejected password must not leave a wallet behind"
        );
        assert!(service.unused_address().await.is_err(), "still not open");

        service.open_or_create_wallet(PASSWORD).await.unwrap();
        assert_eq!(service.seed_words(PASSWORD).await.unwrap().len(), 24);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn change_password_round_trips() {
        let (_dir, service) = open_service().await;

        service
            .change_password(PASSWORD, NEW_PASSWORD)
            .await
            .unwrap();
        // Re-opening the open wallet checks the password in force, which shows which one it is.
        service.open_or_create_wallet(NEW_PASSWORD).await.unwrap();
        assert!(service.open_or_create_wallet(PASSWORD).await.is_err());
        // The seed must still be readable through the rotated key, and only with the new password.
        assert_eq!(service.seed_words(NEW_PASSWORD).await.unwrap().len(), 24);
        assert!(service.seed_words(PASSWORD).await.is_err());

        // The old password follows the rules but is no longer in force, so it gets as far as the
        // wallet, whose own refusal must come out as a wrong password too.
        let err = service
            .change_password(PASSWORD, "Other#Pw1")
            .await
            .unwrap_err();
        assert!(
            is_invalid_password(&err),
            "wrong password must be rejected, got: {err}"
        );
        assert_eq!(status_from(&err).code(), tonic::Code::PermissionDenied);
        service
            .open_or_create_wallet(NEW_PASSWORD)
            .await
            .expect("a failed change must leave the password as it was");

        // The wallet is always password-protected: a new password breaking the rules, the empty
        // one included, is refused and the current one stays in force.
        for weak in ["", "short", "all lowercase 1!"] {
            let err = service
                .change_password(NEW_PASSWORD, weak)
                .await
                .unwrap_err();
            assert!(
                err.downcast_ref::<WeakPassword>().is_some(),
                "{weak:?} must be rejected as a weak password, got: {err}"
            );
        }
        service.open_or_create_wallet(NEW_PASSWORD).await.unwrap();

        // Changing back is a change like any other.
        service
            .change_password(NEW_PASSWORD, PASSWORD)
            .await
            .unwrap();
        service.open_or_create_wallet(PASSWORD).await.unwrap();
    }
    
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_address_requires_the_wallet_password() {
        let (_dir, service) = open_service().await;
        let service = service.with_broadcaster(Arc::new(UnreachableBroadcaster));

        let err = service
            .send_to_address(
                "wrong",
                REGTEST_ADDRESS.parse().unwrap(),
                Amount::from_sat(1_000),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            is_invalid_password(&err),
            "expected InvalidPassword, got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_address_without_a_broadcaster_is_an_error() {
        let (_dir, service) = open_service().await;

        let err = service
            .send_to_address(
                PASSWORD,
                REGTEST_ADDRESS.parse().unwrap(),
                Amount::from_sat(1_000),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no chain backend"), "got: {err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_address_refuses_an_address_on_another_network() {
        let (_dir, service) = open_service().await;
        let service = service.with_broadcaster(Arc::new(UnreachableBroadcaster));

        let err = service
            .send_to_address(
                PASSWORD,
                MAINNET_ADDRESS.parse().unwrap(),
                Amount::from_sat(1_000),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            err.downcast_ref::<ParseError>().is_some(),
            "expected a network mismatch, got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn syncing_keeps_the_confidence_map_in_step() {
        let (_dir, service) = service();
        let service = service.with_chain_data_source(NoopChainDataSource);

        assert!(
            !service.sync_once(&NoopChainDataSource).await.unwrap(),
            "nothing to sync before the wallet is opened"
        );

        service.open_or_create_wallet(PASSWORD).await.unwrap();
        assert!(
            !service.is_ready(),
            "with a chain data source, only a completed sync makes the wallet ready"
        );

        // Nothing on chain, so the map stays empty, but the call must not deadlock — it takes
        // the wallet lock and then the confidence-map lock in that order.
        assert!(service.sync_once(&NoopChainDataSource).await.unwrap());
        assert_eq!(service.utxos().await.unwrap().len(), 0);
    }

    // --- protobuf mapping ---------------------------------------------------------------------
    //
    // These are the seams where a units or signedness mistake would reach bisq2 silently, so
    // they get direct coverage rather than relying on the wallet-level tests.

    use std::str::FromStr as _;

    use ::wallet::wallet_info::{TxInputInfo, TxOutputInfo};
    use bdk_wallet::bitcoin::ScriptBuf;

    fn a_txid() -> Txid {
        Txid::from_str("69111c8de670d7a12b8c4db85c67485889b30335cdd3fd7f18924104e88e9fc3").unwrap()
    }

    #[test]
    fn balance_maps_to_proto_without_losing_the_breakdown() {
        let balance = Balance {
            immature: Amount::from_sat(1),
            trusted_pending: Amount::from_sat(20),
            untrusted_pending: Amount::from_sat(300),
            confirmed: Amount::from_sat(4_000),
        };

        let proto: GetBalanceResponse = balance.clone().into();

        // bisq2 reads only `balance`, and expects the spendable total.
        assert_eq!(proto.balance, balance.trusted_spendable().to_sat());
        assert_eq!(
            proto.balance, 4_020,
            "spendable = confirmed + trusted pending"
        );
        assert_eq!(proto.confirmed, 4_000);
        assert_eq!(proto.trusted_pending, 20);
        assert_eq!(proto.untrusted_pending, 300);
        assert_eq!(proto.immature, 1);
        // Untrusted/immature funds must never be presented as spendable.
        assert!(proto.balance < balance.total().to_sat());
    }

    #[test]
    fn address_maps_to_proto_and_back() {
        let info = address_info(REGTEST_ADDRESS);

        let proto: bmp_wallet::PubAddressInfo = info.clone().into();
        assert_eq!(proto.address, REGTEST_ADDRESS);
        let parsed = Address::<NetworkUnchecked>::try_from(proto).unwrap();
        assert_eq!(&parsed, info.address.as_unchecked());

        // The empty default must not parse either.
        let empty = bmp_wallet::PubAddressInfo::default();
        assert!(Address::<NetworkUnchecked>::try_from(empty).is_err());
        let garbage = bmp_wallet::PubAddressInfo {
            address: "not-an-address".to_owned(),
        };
        assert!(Address::<NetworkUnchecked>::try_from(garbage).is_err());
    }

    #[test]
    fn utxo_maps_to_proto_and_tolerates_an_undecodable_script() {
        let utxo = UtxoInfo {
            tx_id: a_txid(),
            vout: 2,
            amount: Amount::from_sat(50_000),
            address: Some(address_info(REGTEST_ADDRESS)),
            num_confirmations: 6,
            imported: true,
        };
        let proto: bmp_wallet::Utxo = utxo.into();

        assert_eq!(proto.tx_id, a_txid().to_string());
        assert_eq!(proto.vout, 2);
        assert_eq!(proto.amount, 50_000);
        assert_eq!(proto.address.unwrap().address, REGTEST_ADDRESS);
        assert_eq!(proto.num_confirmations, 6);

        // A non-standard script must come through as an absent address, not panic.
        let nameless = UtxoInfo {
            address: None,
            ..UtxoInfo {
                tx_id: a_txid(),
                vout: 0,
                amount: Amount::ZERO,
                address: None,
                num_confirmations: 0,
                imported: false,
            }
        };
        assert_eq!(bmp_wallet::Utxo::from(nameless).address, None);
    }

    #[test]
    fn transaction_maps_to_proto_with_a_seconds_timestamp() {
        let tx = TxInfo {
            tx_id: a_txid(),
            inputs: vec![TxInputInfo {
                prev_out_tx_id: a_txid(),
                prev_out_index: 1,
                // Max sequence: must survive the widening to a signed proto field.
                sequence: u32::MAX,
                script_sig: ScriptBuf::from_bytes(vec![0x51]),
                witness: "0201ab".to_owned(),
            }],
            outputs: vec![TxOutputInfo {
                value: Amount::from_sat(100_000),
                address: Some(address_info(REGTEST_ADDRESS)),
                script_pubkey: ScriptBuf::from_bytes(vec![0x76, 0xa9]),
            }],
            lock_time: 800_000,
            block_height: Some(800_001),
            timestamp: Some(1_700_000_000),
            num_confirmations: 3,
            amount: Amount::from_sat(100_000),
            incoming: true,
        };

        let proto: bmp_wallet::Transaction = tx.into();

        assert_eq!(proto.tx_id, a_txid().to_string());
        assert_eq!(proto.block_height, 800_001);
        assert_eq!(proto.num_confirmations, 3);
        assert_eq!(proto.amount, 100_000);
        assert!(proto.incoming);
        assert_eq!(proto.lock_time, 800_000);

        // The field bisq2 feeds to Instant.ofEpochSecond. Milliseconds here would put the tx
        // ~50,000 years in the future in the UI.
        assert_eq!(proto.date, 1_700_000_000);
        assert!(
            proto.date < 4_000_000_000,
            "date must be seconds, not millis"
        );

        assert_eq!(proto.inputs.len(), 1);
        assert_eq!(proto.inputs[0].prev_out_index, 1);
        assert_eq!(
            proto.inputs[0].sequence_number,
            i64::from(u32::MAX),
            "sequence must widen, not wrap negative"
        );
        assert_eq!(proto.inputs[0].witness, "0201ab");
        assert_eq!(proto.inputs[0].script_sig, vec![0x51]);

        assert_eq!(proto.outputs.len(), 1);
        assert_eq!(proto.outputs[0].value, 100_000);
        assert_eq!(
            proto.outputs[0].address.as_ref().unwrap().address,
            REGTEST_ADDRESS
        );
        assert_eq!(proto.outputs[0].script_pub_key, vec![0x76, 0xa9]);
    }

    #[test]
    fn an_unconfirmed_transaction_maps_to_zeroed_confirmation_fields() {
        let tx = TxInfo {
            tx_id: a_txid(),
            inputs: vec![],
            outputs: vec![],
            lock_time: 0,
            block_height: None,
            timestamp: None,
            num_confirmations: 0,
            amount: Amount::from_sat(42),
            incoming: false,
        };

        let proto: bmp_wallet::Transaction = tx.into();

        // proto3 can't express "absent" for scalars; bisq2 renders 0 as "pending".
        assert_eq!(proto.block_height, 0);
        assert_eq!(proto.date, 0);
        assert_eq!(proto.num_confirmations, 0);
        assert!(!proto.incoming);
    }
}
