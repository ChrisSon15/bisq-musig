//! Smoke test of the `musigd` binary: it starts and serves the two services that bisq2 uses, the
//! trade service `musigrpc.Musig` and, with `--wallet-dir`, the wallet service `wallet.Wallet`.
//!
//! The trade service uses the wallet of the wallet service, so a trade can be created only after
//! the wallet is open.
//!
//! The lock times of the trades are configuration values of `musigd`. It reads them from the
//! environment and does not start with values outside the limits.
//!
//! The tests need no chain. `musigd` creates its Bitcoin Core RPC client at start-up but connects
//! only for a broadcast, and without `--wallet-peer` the wallet does not sync.

use std::io::Read as _;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rpc::pb::bmp_wallet::wallet_client::WalletClient;
use rpc::pb::bmp_wallet::{IsWalletReadyRequest, OpenOrCreateWalletRequest};
use rpc::pb::musigrpc::musig_client::MusigClient;
use rpc::pb::musigrpc::{PubKeySharesRequest, Role};
use tonic::Code;
use tonic::transport::Channel;

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
