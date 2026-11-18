### Rust gRPC interface for the Bisq2 MuSig trade protocol

This is an experimental Rust-based gRPC interface being developed for Bisq's upcoming single-tx trade protocol, along
with Java test clients. The `.proto` files live in [src/main/proto](src/main/proto); the Rust code is generated from
them by `build.rs` (into `src/pb`), the Java code by Maven. The server hosts these services:

| Service          | Proto              | Purpose                                                                                                       |
|------------------|--------------------|---------------------------------------------------------------------------------------------------------------|
| `musigrpc.Musig` | `rpc.proto`        | The trade protocol (see below)                                                                                |
| `wallet.Wallet`  | `bmp_wallet.proto` | Bisq2-facing wallet API backed by a persistent `BMPWallet`; derived from bisq2's own `wallet.proto` (see below) |

`musigd` serves `Musig` always, and `wallet.Wallet` when started with `--wallet-dir`. The trade service uses the wallet
of the wallet service: a trade can be started only when `musigd` runs with `--wallet-dir` and the wallet is open.

The behaviour of the trade protocol is specified in [docs/specifications/trade](../docs/specifications/trade/README.md).

#### Trade protocol (`Musig` service)

The service converts between the protobuf messages and the operations of the trade engine in the `protocol` crate
(`protocol::trade_model`). The engine uses the `musig2` crate to construct aggregated signatures for the traders'
warning and redirect transactions, with pubkey & nonce shares and partial signatures exchanged with the Java client, to
pass them back in as fields of the peer's RPC requests, setting up the trade.

The setup, the normal closure, the custom payout and the force-closure via the swap tx are implemented, on top of the
wallet of the daemon. The service does not broadcast any transaction yet, and the tx confirmation status streams are
mocked. The warning, redirect, claim and penalty transactions are built and signed, but no RPC publishes them yet.

See [MuSig trade protocol messages](musig-trade-protocol-messages.txt) for my current (incomplete) picture of what the
trade messages between the peers would look like, and thus the necessary data to exchange in an RPC interface between
the Bisq2 client and the Rust server managing the wallet and key material.

#### Bisq2-facing wallet (`wallet.Wallet` service)

When `musigd` is started with `--wallet-dir <dir>`, it serves the `wallet.Wallet` service from a `BMPWallet` stored in
that directory. The wallet is not opened at startup: the client opens (or creates) it with the `OpenOrCreateWallet`
RPC, which carries the wallet password. Before that, `IsWalletReady` tells a client, without a password, whether a
wallet database exists in the directory (`exists`) and whether it has been opened (`open`), so it can tell whether
`OpenOrCreateWallet` would open its wallet or create a new, empty one. The wallet syncs over Compact Block Filters
from the peers given with `--wallet-peer <host:port>` (repeatable), re-syncing every `--wallet-poll-secs` seconds;
with no peer it does not sync, but all non-chain operations still work. `--wallet-network` selects the network (default `regtest`). Transactions are
broadcast via a Bitcoin Core RPC connection, by default to `http://localhost:18443` (`--bitcoin-rpc-url`),
authenticated with `--bitcoin-rpc-user` / `--bitcoin-rpc-pass` if given. A `bitcoind` regtest instance may be started
up as follows:

```sh
bitcoind -regtest -prune=0 -txindex=1 -blockfilterindex=1 -peerblockfilters=1 -server \
  -rpcuser=bitcoin -rpcpassword=bitcoin -datadir=.localnet/bitcoind
```

The `-blockfilterindex` and `-peerblockfilters` (compact filters) options are needed for the wallet to sync from this
node.

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

The following Java clients are run from this directory (`rpc`):

3. To build and run the Java gRPC test client to carry out a trade, run:

```sh
mvn install exec:java
```

The client needs two daemons, one for the buyer and one for the seller, each with a wallet directory and an open,
funded wallet. Their ports are given with `-Dbuyer.port=...` (default 50051) and `-Dseller.port=...` (default 50052).

4. To exercise every RPC of the Bisq2-facing wallet (needs `musigd` started with `--wallet-dir`), run:

```sh
mvn exec:java -Pbmp-wallet
```

The host and port can be overridden with `-Dwallet.host=...` / `-Dwallet.port=...`.

### Integration tests using testenv crate

The Java integration tests (`mvn verify`) are:

- `BmpWalletServiceIntegrationTest` — funds and spends a `BMPWallet` through the `wallet.Wallet` service. Needs a running
  testenv; it starts its own `musigd` (step 2 below).
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

It prints its connection details as `TESTENV_*=...` lines. Export them in the shell where you'll run Maven (step 3),
e.g. `TESTENV_RPC_URL`, `TESTENV_RPC_USER`, `TESTENV_RPC_PASS` and `TESTENV_P2P_ADDR`.

2. **Build `musigd`:**

The wallet integration tests launch the `musigd` binary themselves, from `target/debug/musigd` (override with
`-Dmusigd.bin=/path/to/musigd`):

```sh
cargo build --bin musigd
```

3. **Run the Maven test command:**

   This command will compile the Java code and execute the integration tests. Run it from the project's root directory,
   in the shell where you exported the `TESTENV_*` variables from step 1.

   ```sh
   mvn -f rpc/pom.xml clean verify
   ```
