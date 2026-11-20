### Rust gRPC interface for the Bisq2 MuSig trade protocol

This is an experimental Rust-based gRPC interface being developed for Bisq's upcoming single-tx trade protocol, along
with Java test clients. The `.proto` files live in [src/main/proto](src/main/proto); the Rust code is generated from
them by `build.rs` (into `src/pb`), the Java code by Maven. The server hosts these services:

| Service          | Proto              | Purpose                                                                                                       |
|------------------|--------------------|---------------------------------------------------------------------------------------------------------------|
| `musigrpc.Musig` | `rpc.proto`        | The trade protocol (see below)                                                                                |
| `wallet.Wallet`  | `bmp_wallet.proto` | Bisq2-facing wallet API backed by a persistent `BMPWallet`; derived from bisq2's own `wallet.proto` (see below) |

`musigd` serves `Musig` always, and `wallet.Wallet` when started with `--wallet-dir`. The trade service uses the wallet
of the wallet service: every request of a trade needs `musigd` to run with `--wallet-dir` and the wallet to be open.

The engine takes the key shares, keys, addresses and coins of a trade from this wallet. It reserves the coins of the
deposit, and `AbandonTrade` releases them for a trade whose deposit is not signed yet. At the close of a trade the key
of the payout is imported into the wallet. `Close` of `wallet.Wallet` moves the payouts of closed trades to the seed
wallet; bisq2 calls it when the user ends the program. `ReleaseUtxo` of `wallet.Wallet` releases a coin that stayed
reserved without a trade, for example after `musigd` stopped during a setup; `ListUtxos` marks the reserved coins. The rules are in
[docs/specifications/wallet](../docs/specifications/wallet/README.md).

The lock times of the trades are configuration values of `musigd`, not fields of a request. `musigd` reads them from
the environment when it starts (an option of the same meaning has priority):

| Environment variable               | Option                    | Default | Meaning (all values in blocks)                                                |
|------------------------------------|---------------------------|---------|-------------------------------------------------------------------------------|
| `BISQ_MUSIG_WARNING_LOCK_TIME`     | `--warning-lock-time`     | 1440    | Lock time of the warning transactions, after the confirmation of the deposit   |
| `BISQ_MUSIG_CLAIM_LOCK_TIME`       | `--claim-lock-time`       | 720     | Lock time of the claim transactions, after the confirmation of the warning     |
| `BISQ_MUSIG_DEPOSIT_TIMEOUT`       | `--deposit-timeout`       | 144     | Blocks without a confirmed deposit after which a trader aborts the deposit     |
| `BISQ_MUSIG_RELEASE_SAFETY_MARGIN` | `--release-safety-margin` | 6       | The seller does not release when fewer blocks remain before the buyer's warning |
| `BISQ_MUSIG_FORCED_CLOSE_MARGIN`   | `--forced-close-margin`   | the release safety margin | The seller's engine publishes the swap transaction on its own when this number of blocks, or fewer, remain before the buyer's warning |

Both traders must use the same two lock times. With different values the setup of a trade ends before the deposit is
signed. The warning lock time must be larger than 3 plus the release safety margin, and the claim lock time larger
than 1; `musigd` does not start otherwise.

The behaviour of the trade protocol is specified in [docs/specifications/trade](../docs/specifications/trade/README.md).

#### Trade protocol (`Musig` service)

The service converts between the protobuf messages and the operations of the trade engine in the `protocol` crate
(`protocol::trade_model`). The engine uses the `musig2` crate to construct aggregated signatures for the traders'
warning and redirect transactions, with pubkey & nonce shares and partial signatures exchanged with the Java client, to
pass them back in as fields of the peer's RPC requests, setting up the trade.

The setup, the normal closure, the custom payout, the force-closure via the swap tx and the dispute path (warning,
redirect, claim, penalty) are implemented, on top of the wallet of the daemon and of its connection to the peers of the
Bitcoin network.

The service watches every trade on the chain while `musigd` runs, and it sends the transactions of the trade itself:

- the deposit transaction after `PublishDepositTx`, the swap transaction after `CloseTrade` without data, the custom
  payout transaction after `CustomCloseTrade`, and the trader's warning transaction after `PublishWarningTx`;
- without a request: the deposit abort transaction when the deposit transaction was not confirmed within the deposit
  timeout, the swap transaction when the buyer did not answer the release in time, the redirect or the penalty
  transaction when the warning transaction of the peer is confirmed, and the claim transaction when the claim lock time
  of the trader's own warning has passed.

A transaction is sent again after every new block until it is confirmed. `SubscribeTradeEvents` returns the events of
one trade: its state, every transaction that was sent, the confirmations of its transactions, a loss, and a problem
with the peers. Every event also names the deadlines of the trade as block heights, the actions that the user must
take, and the reason if the trade was terminated. The requests that hand out a signature or a key share are refused
until the deposit transaction has 3 confirmations.

The wallet service sends the unconfirmed transactions that spend coins of the wallet again, after the first sync of a
run and after every new block. This includes a drain transaction of `Close` that no peer took before `musigd` ended.

Every trade is stored in the database of the wallet, before a response or a transaction of the trade leaves `musigd`.
When the wallet is opened after a start of `musigd`, the trades are loaded: a trade whose deposit transaction was not
signed is abandoned, and every other trade continues. `musigd` evaluates the blocks since the deposit signature of
each trade, so it finds and answers what happened while it did not run. Until this is done for a trade, a request that
needs the chain is refused and can be repeated. If a stored trade cannot be loaded, every request of the `Musig`
service fails with `DATA_LOSS`; the wallet service keeps working.

Private key shares are not written to the log.

Not implemented: the deposit abort transaction after the deposit timeout, fee increases of sent transactions, and the
restore of trades from the seed or from a backup.

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
with no peer it does not sync, but all non-chain operations still work. `--wallet-network` selects the network (default `regtest`).

All access to the chain goes through these peers, over the Bitcoin peer-to-peer network: the wallet sync, the
observation of the transactions of a trade, and the sending of transactions. One long-running connection serves the
wallet and the trade service. `musigd` connects only to the given peers and has no connection to the RPC interface of
a node. A peer must serve compact block filters. While no peer is reachable, `musigd` keeps trying to connect, the
wallet does not sync, the trade service refuses to publish a deposit, and its confirmation events arrive only when a
peer is reachable again. A `bitcoind` regtest instance
that can serve as the peer may be started up as follows:

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
cargo run --bin musigd -- --wallet-dir /tmp/bmp-wallet --wallet-peer 127.0.0.1:18444
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

The Rust tests in `rpc/tests/test_musigd.rs` start real `musigd` processes. With the directory `MUSIGD_TEST_LOG_DIR` in
the environment, every process writes its log at debug level to a file in that directory.

The blackbox tests in `rpc/tests/blackbox/` play use cases of two users, Alice and Bob. Each user has an own Bitcoin
node and an own `musigd` process, and the two nodes are peers of each other. The plan of these tests is
`rpc/tests/blackbox/BlackPlan.md`. Run them with `cargo test -p rpc --test blackbox`. They write the logs of the
`musigd` processes to `MUSIGD_TEST_LOG_DIR` in the same way, in files named after the user.

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
