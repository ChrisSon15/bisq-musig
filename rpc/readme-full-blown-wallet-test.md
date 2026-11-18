# The full-blown `wallet.Wallet` test

`BmpWalletEndToEndIntegrationTest` (in `src/test/java/bisq/`) drives **every RPC** of the
`wallet.Wallet` gRPC service — the bisq2-facing wallet that `musigd` serves from a `BMPWallet` —
end to end, against a real regtest chain with real transactions mined into real blocks. It is as
close to production as regtest gets: the wallet discovers its funds over compact block filters
from a real `bitcoind`, builds, signs and broadcasts real transactions through Bitcoin Core's RPC,
and sees them confirm.

Unlike the other Java integration tests in this module, it is **self-contained**: it starts its
own regtest node, mines its own chain, launches its own `musigd`, and tears everything down
afterwards. There is no `testenv-server` to start by hand and no `TESTENV_*` variable to export.

## What it covers

The wallet goes through its whole life in eleven ordered steps:

| Step | RPCs exercised | What is proven |
|---|---|---|
| 1 | nine of the ten (all but `OpenOrCreateWallet`) | Every wallet call is refused with `FAILED_PRECONDITION` until `OpenOrCreateWallet` has been called; `IsWalletReady` is `false`. |
| 2 | `OpenOrCreateWallet` | Weak passwords (empty, too short, no upper case, no special character) are refused with `INVALID_ARGUMENT` and the rules spelled out, and leave no wallet behind. A good password creates the SQLCipher database. Re-opening is idempotent with the right password and `PERMISSION_DENIED` with a wrong one. |
| 3 | `IsWalletReady`, `GetBalance`, `ListTransactions`, `ListUtxos`, `GetWalletAddresses` | The wallet becomes ready only after its first compact-block-filter sync over bitcoind's P2P port, and starts out empty in every respect. |
| 4 | `GetSeedWords` | A stable 24-word BIP39 mnemonic, revealed only to the holder of the password. |
| 5 | `GetUnusedAddress`, `GetWalletAddresses` | Fresh regtest addresses, never repeated, listed in the order they were revealed. |
| 6 | `ListUtxos`, `ListTransactions`, `GetBalance`, `GetUnusedAddress` | **tx1**: the node pays two wallet addresses in a single `sendmany` transaction. The wallet discovers it on its own once mined: balance breakdown, both UTXOs, the full transaction (inputs, outputs, height, confirmations, seconds-since-epoch date). Confirmations follow the tip over two more blocks while the block height stays put. bitcoind's `getrawtransaction` view agrees. Used addresses are not handed out again. |
| 7 | `SendToAddress` | Refusals: wrong passphrase (`PERMISSION_DENIED`), missing / malformed / mainnet address (`INVALID_ARGUMENT`), more than the wallet holds ("Insufficient funds"). Balance, UTXO set, history and the node's mempool are untouched. |
| 8 | `SendToAddress`, `ListTransactions`, `ListUtxos`, `GetBalance`, `GetWalletAddresses` | **tx2**: the wallet pays the node 0.3 BTC at an explicit fee rate of 1000 sat/kwu (4 sat/vB). Immediately: the payment is listed unconfirmed, its inputs are tx1 outputs, the change is `trustedPending`, the change address is listed. Then bitcoind's mempool entry must show the same fee and the requested fee rate. After one block: confirmed on both sides, the node's wallet received 0.3 BTC. |
| 9 | `ChangePassword`, `GetSeedWords`, `SendToAddress` | Wrong old password and weak new passwords are refused and change nothing. A real change re-keys the database: only the new password opens the wallet, reveals the seed, or can spend; the balance is untouched. |
| 10 | `OpenOrCreateWallet`, `IsWalletReady`, and all read RPCs | `musigd` is stopped and restarted on the same wallet directory. The retired password no longer opens it; the new one does; after re-syncing, seed, balance, history, UTXOs and addresses are exactly as before. |
| 11 | `SendToAddress`, `ListTransactions`, `ListUtxos`, `GetBalance` | **tx3**: after re-key and restart the wallet pays more than any single UTXO holds, so it must spend every UTXO it has, at the server's default fee rate (500 sat/kwu). Final state: one change UTXO, three transactions, credits minus debits equal the balance, and the node's wallet got paid. |

## Prerequisites

The test has been written for Linux and macOS: stopping `testenv-server` relies on the SIGTERM
that `Process.destroy()` sends there, on which the server shuts down cleanly.

| Tool | Why |
|---|---|
| Rust toolchain (stable) | builds `musigd` and `testenv-server` |
| `protoc` on `PATH` (or `PROTOC` set) | the `rpc` crate generates its gRPC code at build time; `apt-get install -y protobuf-compiler` on Debian/Ubuntu |
| JDK 21 | the Maven module targets Java 21 |
| Maven 3.9+ | runs the test; downloads `protoc` and the gRPC Java plugin itself |
| Internet access on first build | the `testenv` crate's build scripts download Bitcoin Core 29.0 and `electrs` into `target/`; Maven downloads its plugins and dependencies |

No Bitcoin node, no wallet, no Podman: `testenv-server` brings its own `bitcoind` (started with
`-regtest -txindex -blockfilterindex -peerblockfilters`) and `electrs`, in a temporary directory
that is deleted when it stops.

## Build the two binaries

From the project root:

```sh
cargo build --bin musigd
cargo build -p testenv --bin testenv-server
```

The first `testenv` build downloads Bitcoin Core and `electrs`; later builds reuse them. The test
looks for both binaries under `target/debug/`. Rebuild `musigd` after changing anything in the
`rpc` or `wallet` crates — the test runs whatever binary it finds, it does not build it.

## Run the test

From the project root:

```sh
mvn -f rpc/pom.xml verify -Dit.test=BmpWalletEndToEndIntegrationTest
```

or from the `rpc` directory:

```sh
mvn verify -Dit.test=BmpWalletEndToEndIntegrationTest
```

`verify` compiles the Java code (generating the gRPC stubs from `src/main/proto/` on the way),
runs the selected integration test through the Failsafe plugin, and fails the build if the test
fails. The run takes well under a minute on a developer machine; most of it is the two
`musigd` start-ups and the compact-block-filter syncs.

What a successful run prints, abbreviated:

```
Starting testenv-server: /…/target/debug/testenv-server
testenv-server is up: RPC http://127.0.0.1:…, P2P 127.0.0.1:…, electrum 127.0.0.1:…
Mining 101 blocks so the node has spendable coins...
Starting musigd: /…/target/debug/musigd --port 50063 --wallet-dir /tmp/bmp-wallet-e2e-it-… --wallet-network regtest --wallet-poll-secs 2 --bitcoin-rpc-url http://127.0.0.1:… --bitcoin-rpc-user bitcoin --bitcoin-rpc-pass … --wallet-peer 127.0.0.1:…
musigd is accepting connections on port 50063
tx1: node pays bcrt1p… and bcrt1p… in one tx
tx2: wallet pays 30000000 sat to bcrt1q…
tx2: fee 569 sat for 568 wu = 1001.8 sat/kwu
Starting musigd: …
tx3: wallet pays 69899431 sat to bcrt1q… from 2 UTXO(s), after re-key and restart
Final balance: 99600 sat after 3 transactions
[INFO] Tests run: 11, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 29.85 s -- in bisq.BmpWalletEndToEndIntegrationTest
[INFO] BUILD SUCCESS
```

### Options

| Option | Effect |
|---|---|
| `-Dmusigd.bin=/path/to/musigd` | use a `musigd` binary other than `target/debug/musigd` (e.g. a release build) |
| `-Dtestenv.bin=/path/to/testenv-server` | use a `testenv-server` binary other than `target/debug/testenv-server` |
| `-Dit.test=…` left out | run **all** Java integration tests; one of the other two (`BmpWalletServiceIntegrationTest`) needs a `testenv-server` started by hand and the `TESTENV_*` variables exported, see `README.md`; `BmpWalletLifecycleIntegrationTest` needs neither |
| `RUST_LOG=debug` in the environment | more verbose `musigd` and `testenv-server` logs (both default to `info`) |
| `-DskipITs` | skip the integration tests altogether |

### Ports and files

- `musigd` listens on **127.0.0.1:50063** (the other integration tests use 50051, 50052, 50061
  and 50062, so all of them can run in one `mvn verify`). `bitcoind` and `electrs` pick free
  ports, printed in the first lines of the output.
- The wallet lives in a `bmp-wallet-e2e-it-*` directory under the system temp directory, the
  regtest node in a `.tmp*` directory there as well, and the daemon logs in
  `musigd-it-*.log` / `testenv-server-*.log` files next to them. All of it is deleted when the
  test finishes; on a failure, the last 40 lines of the `musigd` and `testenv-server` logs are
  embedded in the assertion message.

## How it is wired

- `TestEnvProcess` (new) launches `testenv-server`, reads the `TESTENV_RPC_URL`,
  `TESTENV_RPC_USER`, `TESTENV_RPC_PASS` and `TESTENV_P2P_ADDR` lines it prints, keeps the rest
  of its output for the log tail, and on close stops it with `Process.destroy()` (SIGTERM), on
  which it shuts down cleanly, taking `bitcoind` and `electrs` with it. Any daemon that is
  nevertheless left behind is killed.
- `MusigdProcess` (shared with the other wallet tests) launches `musigd --wallet-dir … --wallet-peer
  <P2P addr> --wallet-poll-secs 2` and waits for the gRPC port.
- `TestEnvClient` (shared) is the JSON-RPC client for `bitcoind`: mining, `sendmany`, mempool
  entries, `getrawtransaction`/`gettransaction` views used to cross-check the wallet's claims.
- The `musigd.bin` and `testenv.bin` locations are Maven properties in `pom.xml`, passed to the
  test JVM by the Failsafe plugin.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `musigd binary not found at …/target/debug/musigd` | run `cargo build --bin musigd` (or pass `-Dmusigd.bin`) |
| `testenv-server binary not found at …/target/debug/testenv-server` | run `cargo build -p testenv --bin testenv-server` (or pass `-Dtestenv.bin`) |
| `testenv-server did not become ready within 120000ms` | look at the embedded log tail: usually `bitcoind` or `electrs` failed to start. Delete `target/debug/build/corepc-node-*` / `electrsd-*` and rebuild to re-download them. |
| `musigd did not open port 50063` | something else is listening on 50063 (`ss -ltnp | grep 50063`); stop it, or a previous run left a `musigd` behind (`pkill -f 'musigd --port 50063'`). |
| `Timed out … waiting for the wallet to finish its initial sync` | the compact-block-filter sync to `bitcoind` failed; the embedded `musigd` log says why. The wallet peer is `bitcoind`'s P2P port as printed by `testenv-server`. |
| stray `bitcoind` / `electrs` processes after an aborted run | `pkill -f 'bitcoind.*-regtest'` and `pkill electrs`; they only ever belong to a `testenv-server` in a temp directory. |
| `Fee estimation failed` from `sendmany` | would mean `bitcoind` was started without `-fallbackfee`; `testenv-server` sets it, so this points at a `-Dtestenv.bin` that isn't the project's. |
| Maven cannot download `protoc`/plugins | the Java build needs network access on first run; afterwards everything is in `~/.m2`. |
