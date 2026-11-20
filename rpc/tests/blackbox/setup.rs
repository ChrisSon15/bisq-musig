//! The setup that the blackbox tests share: the two users Alice and Bob on a regtest network.
//!
//! Each user has an own Bitcoin node, and an own `musigd` process with a wallet in an own
//! directory. The two nodes are peers of each other. A `musigd` process reaches the chain only
//! through the peer-to-peer port of the node of its user, with compact block filters. The RPC
//! interface of the nodes is for the test code: mining, funding, and reading the chain.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::Context as _;
use bdk_wallet::bitcoin::{Address, Amount, Block, BlockHash, Network, Transaction, Txid};
use rpc::pb::bmp_wallet::wallet_client::WalletClient;
use rpc::pb::bmp_wallet::{
    self, GetBalanceRequest, GetBalanceResponse, GetUnusedAddressRequest, IsWalletReadyRequest,
    ListTransactionsRequest, OpenOrCreateWalletRequest, PubAddressInfo, SendToAddressRequest,
};
use testenv::{Config, TestEnv};
use tokio::time::Instant;
use tonic::transport::Channel;

/// The password of both wallets.
const PASSWORD: &str = "S3cret!!";
/// The amount that each wallet holds after the setup.
pub const FUNDING: Amount = Amount::ONE_BTC;
/// The longest time that the setup waits for a process or for the network.
const WAIT: Duration = Duration::from_secs(90);
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
/// the output is discarded.
fn log_output(user: &str, stream: &str) -> anyhow::Result<Stdio> {
    Ok(match std::env::var_os("MUSIGD_TEST_LOG_DIR") {
        Some(dir) => {
            let path = Path::new(&dir).join(format!("musigd-{user}.{stream}"));
            std::fs::File::create(path)?.into()
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

/// One user: a Bitcoin node, and a `musigd` process that uses this node and holds the wallet of
/// the user.
pub struct User {
    name: &'static str,
    wallet: WalletClient<Channel>,
    // The order of the next three fields is the order in which they are dropped: the process
    // ends before its wallet directory is deleted and before its node stops.
    _musigd: MusigdProcess,
    _wallet_dir: tempfile::TempDir,
    /// The Bitcoin node of the user.
    pub node: TestEnv,
}

impl User {
    /// Starts `musigd` for the user `name` with a new wallet in a new directory, and waits until
    /// the wallet has synced with `node`.
    async fn start(name: &'static str, node: TestEnv) -> anyhow::Result<Self> {
        let wallet_dir = tempfile::tempdir()?;
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
            .arg(wallet_dir.path())
            .arg("--wallet-peer")
            .arg(peer.to_string())
            // The wallet looks for new blocks once per second, so that a test does not wait long.
            .arg("--wallet-poll-secs")
            .arg("1")
            .env("RUST_LOG", log_level)
            .stdout(log_output(name, "out")?)
            .stderr(log_output(name, "err")?)
            .spawn()
            .context("musigd does not start")?;
        let musigd = MusigdProcess(musigd);

        let endpoint = Channel::from_shared(format!("http://127.0.0.1:{port}"))?;
        let channel = wait_for(
            &format!("musigd of {name} accepts a connection"),
            async || Ok(endpoint.connect().await.ok()),
        )
        .await?;
        let mut wallet = WalletClient::new(channel);
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
            name,
            wallet,
            _musigd: musigd,
            _wallet_dir: wallet_dir,
            node,
        })
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

/// The fee of a transaction that is in the chain: the amount of the coins that it spends, less
/// the amount of its outputs. The spent coins are read from `node`.
pub fn fee(node: &TestEnv, tx: &Transaction) -> anyhow::Result<Amount> {
    let mut spent = Amount::ZERO;
    for input in &tx.input {
        let coin = input.previous_output;
        let funding_tx = node
            .bitcoind_client()
            .get_raw_transaction(coin.txid)?
            .transaction()?;
        spent += funding_tx
            .tx_out(usize::try_from(coin.vout)?)
            .with_context(|| format!("the coin {coin} does not exist"))?
            .value;
    }
    let paid: Amount = tx.output.iter().map(|output| output.value).sum();
    spent
        .checked_sub(paid)
        .context("the transaction pays more than it spends")
}
