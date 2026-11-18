//! Smoke test of the `musigd` binary: it starts and serves the two services that bisq2 uses, the
//! trade service `musigrpc.Musig` and, with `--wallet-dir`, the wallet service `wallet.Wallet`.
//!
//! The test needs no chain. `musigd` creates its Bitcoin Core RPC client at start-up but connects
//! only for a broadcast, and without `--wallet-peer` the wallet does not sync.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rpc::pb::bmp_wallet::IsWalletReadyRequest;
use rpc::pb::bmp_wallet::wallet_client::WalletClient;
use rpc::pb::musigrpc::musig_client::MusigClient;
use rpc::pb::musigrpc::{PubKeySharesRequest, Role};
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
        .env("RUST_LOG", "off")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("musigd starts");
    let _musigd = MusigdProcess(musigd);
    let channel = connect(port).await;

    let key_shares = MusigClient::new(channel.clone())
        .init_trade(PubKeySharesRequest {
            trade_id: "musigd-smoke-test".to_owned(),
            my_role: Role::BuyerAsTaker.into(),
        })
        .await
        .expect("musigrpc.Musig is served")
        .into_inner();
    assert_eq!(key_shares.buyer_output_pub_key_share.len(), 33);

    let wallet_state = WalletClient::new(channel)
        .is_wallet_ready(IsWalletReadyRequest {})
        .await
        .expect("wallet.Wallet is served")
        .into_inner();
    assert!(
        !wallet_state.ready,
        "no wallet is open in a new wallet directory"
    );
}
