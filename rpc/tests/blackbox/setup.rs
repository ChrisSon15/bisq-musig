//! The setup that the blackbox tests share: the two users Alice and Bob on a regtest network.
//!
//! Each user has an own Bitcoin node, and an own `musigd` process with a wallet in an own
//! directory. The two nodes are peers of each other. A `musigd` process reaches the chain only
//! through the peer-to-peer port of the node of its user, with compact block filters. The RPC
//! interface of the nodes is for the test code: mining, funding, and reading the chain.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::Context as _;
use bdk_wallet::bitcoin::{
    Address, Amount, Block, BlockHash, Network, SignedAmount, Transaction, TxIn, TxOut, Txid,
};
use rpc::pb::bmp_wallet::wallet_client::WalletClient;
use rpc::pb::bmp_wallet::{
    self, CloseRequest, GetBalanceRequest, GetBalanceResponse, GetUnusedAddressRequest,
    GetWalletAddressesRequest, IsWalletReadyRequest, ListTransactionsRequest,
    OpenOrCreateWalletRequest, PubAddressInfo, SendToAddressRequest,
};
use rpc::pb::musigrpc::musig_client::MusigClient;
use testenv::{Config, TestEnv};
use tokio::time::Instant;
use tonic::transport::Channel;
use wallet::bmp_wallet::{BMPWallet, ImportedKey, WalletApi as _};
use wallet::password::Password;

/// The password of both wallets.
const PASSWORD: &str = "S3cret!!";
/// The amount that each wallet holds after the setup.
pub const FUNDING: Amount = Amount::ONE_BTC;
/// The longest time that a test waits for a process or for the network.
pub const WAIT: Duration = Duration::from_secs(90);
/// The upper limit of a fee that a test accepts: small compared to the amounts of the use cases.
/// The apps choose the fees, so a test knows only a limit for them.
const FEE_LIMIT: Amount = Amount::from_sat(10_000);
/// The time between two checks of a state that is waited for.
const CHECK_PERIOD: Duration = Duration::from_millis(250);

/// Checks `state` again and again until it returns a value. Fails when the value is still
/// missing after [`WAIT`]. `what` describes the awaited state for the error message.
async fn wait_for<T>(
    what: &str,
    mut state: impl AsyncFnMut() -> anyhow::Result<Option<T>>,
) -> anyhow::Result<T> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = state().await? {
            return Ok(value);
        }
        anyhow::ensure!(Instant::now() < deadline, "timeout: {what}");
        tokio::time::sleep(CHECK_PERIOD).await;
    }
}

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
fn free_port() -> anyhow::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Where a `musigd` process writes one of its output streams. With the directory
/// `MUSIGD_TEST_LOG_DIR` in the environment it is a file there, named after the user. Without it
/// the output is discarded. The first process of a user in a test starts the file again, and a
/// later process of the same user adds to it (`append`).
fn log_output(user: &str, stream: &str, append: bool) -> anyhow::Result<Stdio> {
    Ok(match std::env::var_os("MUSIGD_TEST_LOG_DIR") {
        Some(dir) => {
            let path = Path::new(&dir).join(format!("musigd-{user}.{stream}"));
            let mut options = OpenOptions::new();
            options.create(true).append(append).write(true);
            options.truncate(!append).open(path)?.into()
        }
        None => Stdio::null(),
    })
}

/// The balance of a wallet that holds `amount` in confirmed coins and nothing else: no
/// unconfirmed transaction, no coin of a block reward, and no reserved coin.
fn settled_balance(amount: Amount) -> GetBalanceResponse {
    GetBalanceResponse {
        balance: amount.to_sat(),
        confirmed: amount.to_sat(),
        trusted_pending: 0,
        untrusted_pending: 0,
        immature: 0,
        locked: 0,
    }
}

/// The unspent coins of a wallet, by the part of the wallet that holds them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Coins {
    /// The amounts of the coins of the seed wallet, which is the main wallet of the user.
    pub seed: Vec<Amount>,
    /// The amounts of the coins in the wallets of imported keys. They are the payouts of closed
    /// trades.
    pub imported: Vec<Amount>,
}

/// A running `musigd` process and the clients of its two services.
struct App {
    musigd: MusigdProcess,
    wallet: WalletClient<Channel>,
    musig: MusigClient<Channel>,
}

impl App {
    /// Starts `musigd` for the user `name` on a wallet directory, opens the wallet, which creates
    /// it if the directory is empty, and waits until the wallet has synced with `node`.
    /// `first` tells whether this is the first process of the user in the test.
    async fn start(
        name: &str,
        wallet_dir: &Path,
        node: &TestEnv,
        first: bool,
    ) -> anyhow::Result<Self> {
        let port = free_port()?;
        let peer = node
            .p2p_socket_addr()
            .context("the node has no peer port")?;
        let log_level = if std::env::var_os("MUSIGD_TEST_LOG_DIR").is_some() {
            "debug"
        } else {
            "off"
        };
        let musigd = Command::new(env!("CARGO_BIN_EXE_musigd"))
            .arg("--port")
            .arg(port.to_string())
            .arg("--wallet-dir")
            .arg(wallet_dir)
            .arg("--wallet-peer")
            .arg(peer.to_string())
            // The wallet looks for new blocks once per second, so that a test does not wait long.
            .arg("--wallet-poll-secs")
            .arg("1")
            .env("RUST_LOG", log_level)
            .stdout(log_output(name, "out", !first)?)
            .stderr(log_output(name, "err", !first)?)
            .spawn()
            .context("musigd does not start")?;
        let musigd = MusigdProcess(musigd);

        let endpoint = Channel::from_shared(format!("http://127.0.0.1:{port}"))?;
        let channel = wait_for(
            &format!("musigd of {name} accepts a connection"),
            async || Ok(endpoint.connect().await.ok()),
        )
        .await?;
        let mut wallet = WalletClient::new(channel.clone());
        wallet
            .open_or_create_wallet(OpenOrCreateWalletRequest {
                password: PASSWORD.to_owned(),
            })
            .await?;
        wait_for(&format!("the wallet of {name} is ready"), async || {
            let state = wallet.is_wallet_ready(IsWalletReadyRequest {}).await?;
            Ok(state.into_inner().ready.then_some(()))
        })
        .await?;

        Ok(Self {
            musigd,
            wallet,
            musig: MusigClient::new(channel),
        })
    }
}

/// One user: a Bitcoin node, and the app of the user. The app is a `musigd` process that uses
/// this node and holds the wallet of the user.
pub struct User {
    name: &'static str,
    wallet: WalletClient<Channel>,
    /// The trade service of the app.
    pub musig: MusigClient<Channel>,
    // The order of the next three fields is the order in which they are dropped: the process
    // ends before its wallet directory is deleted and before its node stops.
    /// `None` while the app is closed. The two clients do not work then.
    musigd: Option<MusigdProcess>,
    wallet_dir: tempfile::TempDir,
    /// The Bitcoin node of the user.
    pub node: TestEnv,
}

impl User {
    /// Starts the app of the user `name` with a new wallet in a new directory.
    async fn start(name: &'static str, node: TestEnv) -> anyhow::Result<Self> {
        let wallet_dir = tempfile::tempdir()?;
        let app = App::start(name, wallet_dir.path(), &node, true).await?;
        Ok(Self {
            name,
            wallet: app.wallet,
            musig: app.musig,
            musigd: Some(app.musigd),
            wallet_dir,
            node,
        })
    }

    /// The user ends the app in the regular way. The app asks the wallet service to close, which
    /// moves the payouts of closed trades to the seed wallet with a drain transaction. Then the
    /// `musigd` process ends. Returns the ID of the drain transaction, or `None` if there was
    /// nothing to move.
    pub async fn close_app(&mut self) -> anyhow::Result<Option<Txid>> {
        let response = self.wallet.close(CloseRequest {}).await?.into_inner();
        self.stop_app();
        Ok(response.drain_tx_id.map(|txid| txid.parse()).transpose()?)
    }

    /// Ends the `musigd` process of the user at once, as a crash does. The wallet service is not
    /// asked to close, so no payout is moved. The wallet directory stays.
    pub fn stop_app(&mut self) {
        self.musigd = None;
    }

    /// Opens the wallet of the user with the `wallet` crate, to look at its internal structure.
    /// The wallet service does not show in which part of the wallet a coin is, and it hands out
    /// no key. The app must be stopped, so that only one process has the database of the wallet
    /// open. The wallet is as the app stored it last.
    fn open_wallet(&self) -> anyhow::Result<BMPWallet> {
        anyhow::ensure!(self.musigd.is_none(), "the app of {} runs", self.name);
        Ok(BMPWallet::load_wallet(
            self.wallet_dir.path().into(),
            Network::Regtest,
            &Password::check(PASSWORD)?,
        )?)
    }

    /// The unspent coins of the wallet, as the wallet itself assigns them to the seed wallet or
    /// to the wallet of an imported key. The app must be stopped (see [`Self::open_wallet`]).
    pub fn coins(&self) -> anyhow::Result<Coins> {
        let mut coins = Coins::default();
        for utxo in self.open_wallet()?.list_utxos() {
            let amounts = if utxo.imported {
                &mut coins.imported
            } else {
                &mut coins.seed
            };
            amounts.push(utxo.amount);
        }
        Ok(coins)
    }

    /// The keys that the wallet of the user has imported: the keys of the payouts of closed
    /// trades. The app must be stopped (see [`Self::open_wallet`]).
    pub fn imported_keys(&self) -> anyhow::Result<Vec<ImportedKey>> {
        Ok(self.open_wallet()?.imported_keys())
    }

    /// The user starts the app again: a new `musigd` process on the wallet directory of the
    /// user.
    pub async fn open_app(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.musigd.is_none(), "the app of {} runs", self.name);
        let app = App::start(self.name, self.wallet_dir.path(), &self.node, false).await?;
        self.wallet = app.wallet;
        self.musig = app.musig;
        self.musigd = Some(app.musigd);
        Ok(())
    }

    /// An address of the wallet that has received no coins yet.
    pub async fn new_address(&mut self) -> anyhow::Result<Address> {
        let address = self
            .wallet
            .get_unused_address(GetUnusedAddressRequest {})
            .await?
            .into_inner()
            .address
            .context("the response has no address")?
            .address;
        Ok(address
            .parse::<Address<_>>()?
            .require_network(Network::Regtest)?)
    }

    /// Sends `amount` from the wallet to `address`, at the default fee rate of the wallet.
    /// Returns the ID of the transaction.
    pub async fn send(&mut self, address: &Address, amount: Amount) -> anyhow::Result<Txid> {
        let response = self
            .wallet
            .send_to_address(SendToAddressRequest {
                passphrase: PASSWORD.to_owned(),
                address: Some(PubAddressInfo {
                    address: address.to_string(),
                }),
                amount: amount.to_sat(),
                fee_rate_per_kwu: None,
            })
            .await?
            .into_inner();
        Ok(response.tx_id.parse()?)
    }

    /// Waits until the wallet holds exactly `amount`, all of it in confirmed coins (see
    /// [`settled_balance`]).
    pub async fn wait_for_balance(&mut self, amount: Amount) -> anyhow::Result<()> {
        let expected = settled_balance(amount);
        let mut last = None;
        let reached = wait_for("balance", async || {
            let balance = self.wallet.get_balance(GetBalanceRequest {}).await?;
            let balance = last.insert(balance.into_inner());
            Ok((*balance == expected).then_some(()))
        })
        .await;
        reached.with_context(|| {
            format!(
                "the wallet of {} does not reach {expected:?}, it reports {last:?}",
                self.name
            )
        })
    }

    /// The amount by which the transaction `tx` of the chain reduced the coins of the wallet:
    /// the amount of the coins at addresses of the wallet that `tx` spends, less the amount of
    /// the outputs of `tx` to addresses of the wallet. It is negative if the wallet gained. The
    /// amount is calculated from the chain; the wallet only names its addresses. These include
    /// the addresses of imported keys, so the payout of a closed trade counts as a coin of the
    /// wallet.
    pub async fn loss_by(&mut self, tx: &Transaction) -> anyhow::Result<SignedAmount> {
        let addresses = self
            .wallet
            .get_wallet_addresses(GetWalletAddressesRequest {})
            .await?
            .into_inner()
            .addresses;
        let mut scripts = HashSet::new();
        for info in addresses {
            let address = info.address.parse::<Address<_>>()?;
            scripts.insert(address.require_network(Network::Regtest)?.script_pubkey());
        }
        let own_amount = |coins: &[TxOut]| -> anyhow::Result<SignedAmount> {
            let own = coins
                .iter()
                .filter(|coin| scripts.contains(&coin.script_pubkey));
            Ok(own.map(|coin| coin.value).sum::<Amount>().to_signed()?)
        };
        let spent = own_amount(&spent_coins(&self.node, tx)?)?;
        Ok(spent - own_amount(&tx.output)?)
    }

    /// The transaction `txid` as the wallet lists it, or `None` if the wallet does not list it.
    pub async fn transaction(
        &mut self,
        txid: Txid,
    ) -> anyhow::Result<Option<bmp_wallet::Transaction>> {
        let transactions = self
            .wallet
            .list_transactions(ListTransactionsRequest {})
            .await?
            .into_inner()
            .transactions;
        let txid = txid.to_string();
        Ok(transactions.into_iter().find(|tx| tx.tx_id == txid))
    }
}

/// Alice and Bob, each with a node, a `musigd` process and a wallet that holds [`FUNDING`].
pub struct Setup {
    pub alice: User,
    pub bob: User,
}

impl Setup {
    /// Starts the two nodes and the two `musigd` processes, and funds both wallets.
    pub async fn new() -> anyhow::Result<Self> {
        let mut alices_node = TestEnv::new()?;
        let bobs_node = start_peer_node(&alices_node)?;
        // The coins of the test come from the block rewards of the node of Alice. A reward can
        // be spent after 100 more blocks. The blocks are mined before the `musigd` processes
        // connect, so that their wallets start on the chain that both nodes share.
        let tip = alices_node.mine_blocks(101)?[100];
        wait_for_tip(&bobs_node, tip).await?;

        let mut setup = Self {
            alice: User::start("Alice", alices_node).await?,
            bob: User::start("Bob", bobs_node).await?,
        };
        for address in [
            setup.alice.new_address().await?,
            setup.bob.new_address().await?,
        ] {
            setup.alice.node.fund_address(&address, FUNDING)?;
        }
        setup.mine_block().await?;
        setup.alice.wait_for_balance(FUNDING).await?;
        setup.bob.wait_for_balance(FUNDING).await?;
        Ok(setup)
    }

    /// Mines one block on the node of Alice, with the transactions that this node holds, and
    /// waits until the node of Bob has the block too. Returns the block as the node of Bob holds
    /// it.
    pub async fn mine_block(&mut self) -> anyhow::Result<Block> {
        let hash = self.alice.node.mine_block()?;
        wait_for_tip(&self.bob.node, hash).await?;
        Ok(self.bob.node.bitcoind_client().get_block(hash)?)
    }

    /// Waits until both nodes hold the unconfirmed transactions `txids`, and mines one block.
    /// Returns the transactions as the block contains them, in the order of `txids`. Fails if
    /// the block does not contain all of them.
    pub async fn mine(&mut self, txids: &[Txid]) -> anyhow::Result<Vec<Transaction>> {
        for txid in txids {
            self.wait_for_unconfirmed(*txid).await?;
        }
        let block = self.mine_block().await?;
        txids
            .iter()
            .map(|txid| {
                let mined = block.txdata.iter().find(|tx| tx.compute_txid() == *txid);
                mined
                    .cloned()
                    .with_context(|| format!("the block does not contain the transaction {txid}"))
            })
            .collect()
    }

    /// Waits until both nodes hold the unconfirmed transaction `txid`: the node that received it
    /// has passed it on to its peer.
    async fn wait_for_unconfirmed(&self, txid: Txid) -> anyhow::Result<()> {
        for user in [&self.alice, &self.bob] {
            let what = format!("the node of {} holds the transaction {txid}", user.name);
            wait_for(&what, async || {
                Ok(user.node.mempool_contains(&txid)?.then_some(()))
            })
            .await?;
        }
        Ok(())
    }
}

/// Starts one more node, which connects to `node` as its peer.
fn start_peer_node(node: &TestEnv) -> anyhow::Result<TestEnv> {
    let peer = node
        .p2p_socket_addr()
        .context("the node has no peer port")?;
    let add_node = format!("-addnode={peer}");
    let mut config = Config::default();
    config.bitcoind.args.push(&add_node);
    // A test environment waits for a lock of the process, so that the tests run one after the
    // other. The first node of the setup holds this lock. The second node must not wait for it:
    // it would wait forever.
    config.runmultithreaded = true;
    TestEnv::new_with_conf(config)
}

/// Waits until the last block of the chain of `node` is `tip`.
async fn wait_for_tip(node: &TestEnv, tip: BlockHash) -> anyhow::Result<()> {
    wait_for(&format!("a node has the block {tip}"), async || {
        Ok((node.best_block_hash()? == tip).then_some(()))
    })
    .await
}

/// Checks that `fee`, which the test names with `what`, is positive and not above [`FEE_LIMIT`].
pub fn assert_small_fee(fee: Amount, what: &str) {
    assert!(
        Amount::ZERO < fee && fee <= FEE_LIMIT,
        "{what} of {fee} is positive and small"
    );
}

/// The coins that a transaction of the chain spends, in the order of its inputs. They are read
/// from `node`.
fn spent_coins(node: &TestEnv, tx: &Transaction) -> anyhow::Result<Vec<TxOut>> {
    let spent_coin = |input: &TxIn| {
        let coin = input.previous_output;
        let funding_tx = node
            .bitcoind_client()
            .get_raw_transaction(coin.txid)?
            .transaction()?;
        let spent = funding_tx.tx_out(usize::try_from(coin.vout)?);
        Ok(spent
            .with_context(|| format!("the coin {coin} does not exist"))?
            .clone())
    };
    tx.input.iter().map(spent_coin).collect()
}

/// The fee of a transaction that is in the chain: the amount of the coins that it spends, less
/// the amount of its outputs. The spent coins are read from `node`.
pub fn fee(node: &TestEnv, tx: &Transaction) -> anyhow::Result<Amount> {
    let spent: Amount = spent_coins(node, tx)?.iter().map(|coin| coin.value).sum();
    let paid: Amount = tx.output.iter().map(|output| output.value).sum();
    spent
        .checked_sub(paid)
        .context("the transaction pays more than it spends")
}
