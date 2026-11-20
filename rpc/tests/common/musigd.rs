//! `musigd` processes for tests, and the clients of their two services.

use std::fs::OpenOptions;
use std::net::{SocketAddrV4, TcpListener};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::Context as _;
use bdk_wallet::bitcoin::{Address, Network};
use rpc::pb::bmp_wallet::wallet_client::WalletClient;
use rpc::pb::bmp_wallet::{GetUnusedAddressRequest, OpenOrCreateWalletRequest};
use rpc::pb::musigrpc::musig_client::MusigClient;
use tokio::time::Instant;
use tonic::transport::Channel;

/// The password of every wallet.
pub const PASSWORD: &str = "S3cret!!";
/// The longest time that a test waits for a process or for the network.
pub const WAIT: Duration = Duration::from_secs(90);
/// The time between two checks of a state that is waited for.
const CHECK_PERIOD: Duration = Duration::from_millis(250);

/// Checks `state` again and again until it returns a value. Fails when the value is still
/// missing after [`WAIT`]. `what` describes the awaited state for the error message.
pub async fn wait_for<T>(
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

/// A `musigd` process. It is ended at once when the value is dropped, as a crash does, so no
/// process is left when a test ends or fails. [`Self::shut_down`] ends it in the regular way.
pub struct MusigdProcess(pub Child);

impl Drop for MusigdProcess {
    fn drop(&mut self) {
        // Errors are ignored, because the process may have ended already.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl MusigdProcess {
    /// Ends the process in the regular way, as `docs/specifications/trade/shutdown.md` defines
    /// it: sends the signal SIGTERM and waits until the process has ended. Fails if the process
    /// does not end, or if its exit status is not 0.
    pub async fn shut_down(mut self) -> anyhow::Result<()> {
        // The standard library can only end a process at once, so the `kill` program of the
        // system sends the signal.
        let sent = Command::new("kill")
            .arg("-TERM")
            .arg(self.0.id().to_string())
            .status()
            .context("the kill program does not start")?;
        anyhow::ensure!(sent.success(), "SIGTERM was not sent to musigd: {sent}");
        let status = wait_for("musigd ends after SIGTERM", async || {
            Ok(self.0.try_wait()?)
        })
        .await?;
        anyhow::ensure!(status.success(), "musigd ended with {status}");
        Ok(())
    }
}

/// Returns a loopback port that was free a moment ago.
pub fn free_port() -> anyhow::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Connects to the `musigd` process that serves on `port` of the loopback address, and tries
/// again while the process starts.
pub async fn connect(port: u16) -> anyhow::Result<Channel> {
    let endpoint = Channel::from_shared(format!("http://127.0.0.1:{port}"))?;
    wait_for(
        &format!("musigd accepts a connection on port {port}"),
        async || Ok(endpoint.connect().await.ok()),
    )
    .await
}

/// Where a `musigd` process writes one of its output streams. With the directory
/// `MUSIGD_TEST_LOG_DIR` in the environment it is a file there, named after the trader. Without
/// it the output is discarded. The first process of a trader in a test starts the file again, and
/// a later process of the same trader adds to it (`append`).
fn log_output(trader: &str, stream: &str, append: bool) -> anyhow::Result<Stdio> {
    Ok(match std::env::var_os("MUSIGD_TEST_LOG_DIR") {
        Some(dir) => {
            let path = Path::new(&dir).join(format!("musigd-{trader}.{stream}"));
            let mut options = OpenOptions::new();
            options.create(true).append(append).write(true);
            options.truncate(!append).open(path)?.into()
        }
        None => Stdio::null(),
    })
}

/// How a `musigd` process of a trader is started.
pub struct Launch<'a> {
    /// The name of the trader. The log files are named after it.
    pub trader: &'a str,
    /// The directory of the wallet. The wallet is created if the directory is empty.
    pub wallet_dir: &'a Path,
    /// The peer-to-peer address of a Bitcoin node. It is the only connection of the process to
    /// the chain: no option for the RPC interface of a node exists.
    pub peer: SocketAddrV4,
    /// Configuration values of the trade engine, as environment variables of the process.
    pub config: &'a [(&'a str, String)],
    /// Whether this is the first process of the trader in the test.
    pub first: bool,
}

/// A running `musigd` process with an open wallet, and the clients of its two services.
pub struct App {
    pub process: MusigdProcess,
    pub wallet: WalletClient<Channel>,
    pub musig: MusigClient<Channel>,
}

impl App {
    /// Starts `musigd` and opens the wallet, as bisq2 does at its start. The wallet is not synced
    /// with the chain yet when this returns.
    pub async fn start(launch: &Launch<'_>) -> anyhow::Result<Self> {
        let port = free_port()?;
        let log_level = if std::env::var_os("MUSIGD_TEST_LOG_DIR").is_some() {
            "debug"
        } else {
            "off"
        };
        let mut command = Command::new(env!("CARGO_BIN_EXE_musigd"));
        for (variable, value) in launch.config {
            command.env(variable, value);
        }
        let process = command
            .arg("--port")
            .arg(port.to_string())
            .arg("--wallet-dir")
            .arg(launch.wallet_dir)
            .arg("--wallet-peer")
            .arg(launch.peer.to_string())
            // The wallet looks for new blocks once per second, so that a test does not wait long.
            .arg("--wallet-poll-secs")
            .arg("1")
            .env("RUST_LOG", log_level)
            .stdout(log_output(launch.trader, "out", !launch.first)?)
            .stderr(log_output(launch.trader, "err", !launch.first)?)
            .spawn()
            .context("musigd does not start")?;
        let process = MusigdProcess(process);

        let channel = connect(port).await?;
        let mut wallet = WalletClient::new(channel.clone());
        wallet
            .open_or_create_wallet(OpenOrCreateWalletRequest {
                password: PASSWORD.to_owned(),
            })
            .await?;
        Ok(Self {
            process,
            wallet,
            musig: MusigClient::new(channel),
        })
    }
}

/// An address of the wallet that has received no coins yet.
pub async fn new_address(wallet: &mut WalletClient<Channel>) -> anyhow::Result<Address> {
    let address = wallet
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
