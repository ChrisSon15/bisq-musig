### Rust gRPC interface for the Bisq2 MuSig trade protocol

This is an experimental Rust-based gRPC interface being developed for Bisq's upcoming single-tx trade protocol, along
with Java test clients. The `.proto` files live in [src/main/proto](src/main/proto); the Rust code is generated from
them by `build.rs` (into `src/pb`), the Java code by Maven. The server hosts these services:

| Service                               | Proto                | Purpose                                                                                                                        |
|---------------------------------------|----------------------|--------------------------------------------------------------------------------------------------------------------------------|
| `musigrpc.Musig`                      | `rpc.proto`          | The original trade protocol mock-up (see below)                                                                                |
| `bmp_protocol.BmpProtocolService`     | `bmp_protocol.proto` | Round-based (`Initialize`, `ExecuteRound1`…`5`) interface onto the `protocol` crate's `BMPProtocol`                            |
| `walletrpc.Wallet`                    | `wallet.proto`       | Experimental wallet and chain notification API, used by `musig-cli`                                                            |
| `wallet.Wallet`                       | `bmp_wallet.proto`   | Bisq2-facing wallet API backed by a persistent `BMPWallet`; derived from bisq2's own `wallet.proto` (see below)                 |

`musigd` serves `Musig` and `walletrpc.Wallet` always, and `wallet.Wallet` when started with `--wallet-dir`.
`BmpProtocolService` is currently only served by the test daemon in [tests/test_bmp_service.rs](tests/test_bmp_service.rs)
(see [Integration tests](#integration-tests-using-testenv-crate)).

#### Trade protocol mock-up (`Musig` service)

The Rust code uses the `musig2` crate to construct aggregated signatures for the traders' warning and redirect
transactions, with pubkey & nonce shares and partial signatures exchanged with the Java client, to pass them back in as
fields of the simulated peer's RPC requests, setting up the trade.

The adaptor logic, multiparty signing and simulated steps for the whole of the trade (normal closure, custom payout and
force-closure via the swap tx) are implemented for the mockup. The txs are built with the `protocol` crate's tx
builders, but on top of mock trade wallets, and the tx confirmation status streams are mocked. None of the mediation or
arbitration paths are implemented or mocked yet.

See [MuSig trade protocol messages](musig-trade-protocol-messages.txt) for my current (incomplete) picture of what the
trade messages between the peers would look like, and thus the necessary data to exchange in an RPC interface between
the Bisq2 client and the Rust server managing the wallet and key material.

#### Experimental wallet gRPC interface and test CLI + Java client (`walletrpc.Wallet` service)

To help test and develop the wallet and chain notification API that will be needed by Bisq, a small Rust gRPC client
with a command-line interface is also included as a binary target (`musig-cli`). Currently, this is providing access to
a handful of experimental wallet RPC endpoints that will talk to BDK to get account balance, new addresses, UTXO set,
tx confidence notifications, etc. (only partially implemented). Run `cargo run -- --help` for the available commands.

A non-interactive Java test gRPC client has also been written to query the UTXO set, then open an RPC stream for each
UTXO and listen for confidence updates (confirmations, reorgs, etc.), running for a few seconds.

This wallet is currently just hardwired to use _regtest_ with a fixed descriptor, without persistence. It uses the
`bdk_bitcoind_rpc` crate to talk to a `bitcoind` instance via JSON-RPC, by default at `http://localhost:18443`
(`--bitcoin-rpc-url`), authenticated with `--bitcoin-rpc-user` / `--bitcoin-rpc-pass` if given. It does a full scan
once upon startup, then polls once per second. A `bitcoind` regtest instance may be started up as follows:

```sh
bitcoind -regtest -prune=0 -txindex=1 -blockfilterindex=1 -peerblockfilters=1 -server \
  -rpcuser=bitcoin -rpcpassword=bitcoin -datadir=.localnet/bitcoind
```

The `-blockfilterindex` and `-peerblockfilters` (compact filters) options are needed if you also want the `BMPWallet`
below to sync from this node.

#### Bisq2-facing wallet (`wallet.Wallet` service)

When `musigd` is started with `--wallet-dir <dir>`, it serves the `wallet.Wallet` service from a `BMPWallet` stored in
that directory. The wallet is not opened at startup: the client opens (or creates) it with the `OpenOrCreateWallet`
RPC, which carries the wallet password. Before that, `IsWalletReady` tells a client, without a password, whether a
wallet database exists in the directory (`exists`) and whether it has been opened (`open`), so it can tell whether
`OpenOrCreateWallet` would open its wallet or create a new, empty one. The wallet syncs over Compact Block Filters
from the peers given with `--wallet-peer <host:port>` (repeatable), re-syncing every `--wallet-poll-secs` seconds;
with no peer it does not sync, but all non-chain operations still work. `--wallet-network` selects the network (default `regtest`). Transactions are
broadcast via the Bitcoin Core RPC connection above.

[bmp_wallet.proto](src/main/proto/bmp_wallet.proto) is derived from bisq2's own `wallet/src/main/proto/wallet.proto`
(same package, service name and field numbers), but it is not a drop-in replacement yet. The wallet is always
password-protected, so `EncryptWallet` / `DecryptWallet` / `IsWalletEncrypted` are gone, replaced by the authenticated
`ChangePassword`, and `SendToAddress` requires the passphrase; `GetNewAddress` is gone, `GetSeedWords` takes the wallet
password, and addresses travel as `PubAddressInfo` messages rather than bare strings. A password set through
`OpenOrCreateWallet` (when it creates the wallet) or `ChangePassword` must follow the password rules documented on
`OpenOrCreateWalletRequest` in the proto; one that doesn't is refused with `INVALID_ARGUMENT`, whose message states the
rules in plain English for the user. These divergences are listed at the top of the proto and are meant to be upstreamed
into bisq2; until then, bisq2's `WalletGrpcClient` needs matching changes to talk to this service.

### Building and running the code

The Rust gRPC server listens on localhost port 50051 by default (`--port`). See `cargo run --bin musigd -- --help` for
all options.

1. To successfully build the Rust server, the `protoc` compiler must be installed separately. Make sure it is on the
   current path, or the `PROTOC` environment variable is set to the path of the binary. It can be installed with your
   package manager (e.g. `apt-get install -y protobuf-compiler`) or downloaded from:

> https://github.com/protocolbuffers/protobuf/releases

2. To build and run the Rust server, run:

```sh
cargo run --bin musigd
```

or, to also serve the Bisq2-facing wallet:

```sh
cargo run --bin musigd -- --wallet-dir /tmp/bmp-wallet --wallet-peer 127.0.0.1:18444 \
  --bitcoin-rpc-user bitcoin --bitcoin-rpc-pass bitcoin
```

3. To build and run the Rust wallet CLI client (default-run), just run:

```sh
cargo run -- wallet-balance
```

The following Java clients are run from this directory (`rpc`):

4. To build and run the Java gRPC test client to carry out a mock trade, run:

```sh
mvn install exec:java
```

5. To subsequently run the Java test client for the wallet gRPC interface, run:

```sh
mvn exec:java -Pwallet
```

6. To exercise every RPC of the Bisq2-facing wallet (needs `musigd` started with `--wallet-dir`), run:

```sh
mvn exec:java -Pbmp-wallet
```

The host and port can be overridden with `-Dwallet.host=...` / `-Dwallet.port=...`.

7. To run the `BmpClient` against the `BmpProtocolService` (needs the test daemon from the next section on port 50051):

```bash
mvn exec:java -Pbmp
```

### Integration tests using testenv crate

The Java integration tests (`mvn verify`) are:

- `BmpServiceIntegrationTest` — simulates a complete trade via the `BmpProtocolService` and verifies it on-chain.
  Needs a running testenv and two externally started test daemons (steps 1 & 2 below).
- `BmpWalletServiceIntegrationTest` — funds and spends a `BMPWallet` through the `wallet.Wallet` service. Needs a running
  testenv; it starts its own `musigd` (step 3 below).
- `BmpWalletLifecycleIntegrationTest` — opening, password changes and restarts of the `BMPWallet`. Starts its own
  `musigd` and needs no chain.
- `BmpWalletEndToEndIntegrationTest` — every RPC of the `wallet.Wallet` service against a regtest chain, with three
  real transactions, a password change and a restart. Self-contained: starts its own `testenv-server` and `musigd`,
  so it needs none of the steps below — see [readme-full-blown-wallet-test.md](readme-full-blown-wallet-test.md).

1. **Start the testenv-server binary crate**

The integration tests require you to have a running instance of the testenv binary crate.
To do so run the following command:

```sh
cargo run --bin testenv-server -p testenv
```

It prints its connection details as `TESTENV_*=...` lines. Export them in the shell where you'll run Maven (step 4),
e.g. `TESTENV_RPC_URL`, `TESTENV_RPC_USER`, `TESTENV_RPC_PASS` and `TESTENV_P2P_ADDR`. Note the `TESTENV_RPC_URL` and
`TESTENV_ELECTRUM_URL` for step 2 as well.

2. **Start two test daemons:**

`BmpServiceIntegrationTest` requires two server instances to represent the two parties in the trade (Alice and Bob).
They are started from an ignored test case in [tests/test_bmp_service.rs](tests/test_bmp_service.rs), as `musigd` doesn't serve
the `BmpProtocolService` yet. Run these commands from the project's root directory, replacing `RPC_URL` and
`ELECTRUM_URL` with the values printed in step 1. It's best to run them in separate terminal windows so you can monitor
their output.

*   Server for Bob (port 50051):
    ```sh
    ELECTRUM_URL=127.0.0.1:33575 RPC_URL=http://127.0.0.1:46111 MUSIGD_PORT=50051 cargo test -p rpc --test test_bmp_service -- --ignored run_musigd_server --nocapture
    ```
*   Server for Alice (port 50052):
    ```sh
    ELECTRUM_URL=127.0.0.1:33575 RPC_URL=http://127.0.0.1:46111 MUSIGD_PORT=50052 cargo test -p rpc --test test_bmp_service -- --ignored run_musigd_server --nocapture
    ```

3. **Build `musigd`:**

The wallet integration tests launch the `musigd` binary themselves, from `target/debug/musigd` (override with
`-Dmusigd.bin=/path/to/musigd`):

```sh
cargo build --bin musigd
```

4. **Run the Maven test command:**

   This command will compile the Java code and execute the integration tests. Run it from the project's root directory,
   in the shell where you exported the `TESTENV_*` variables from step 1.

   ```sh
   mvn -f rpc/pom.xml clean verify
   ```
