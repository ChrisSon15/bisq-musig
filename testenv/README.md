# Bitcoin Regtest Environment

A clean Bitcoin regtest environment using electrsd with automatic executable downloads.

## Features

- **Automatic Downloads**: Downloads required executables (bitcoind, electrs) automatically
- **Dependencies**: No Docker for running the tests
- **Modern Rust API**: Clean, ergonomic interface inspired by BDK
- **Compact Block Filters**: bitcoind runs with `-blockfilterindex=1 -peerblockfilters=1 -txindex=1` and P2P enabled,
  so wallets can sync over CBF as well as via Electrum
- **Web UI**: Built-in blockchain explorer for visual debugging
  (needs podman)
- **Standalone server**: the `testenv-server` binary runs the environment outside of a test, e.g. for the Java
  integration tests

## Quick Start

### Basic Usage

Inside your test use TestEnv.

```rust,ignore
// Create environment (automatically downloads executables)
let mut env = TestEnv::new()?;
env.mine_block()?;

// Create and fund an address
let address = env.new_address()?;
let txid = env.fund_address(&address, Amount::from_sat(100000))?;

// Wait until electrum sees the transaction, then confirm it
env.wait_for_tx(txid)?;
env.mine_block()?;
env.wait_for_block()?;

println!("Transaction confirmed: {}", txid);
```

The `wait_for_*` methods poll until the configured `Config::timeout` (5 seconds by default) expires.

### Web UI (btc-rpc-explorer)

The test environment provides a web-based blockchain explorer for visual debugging.

#### Step 1: tell Testenv, that you want to debug

```rust,ignore
// Create environment
let mut env = TestEnv::new()?;

// Start the Container with the web-ui in a separate process.
// only include this line if you want to debug. do not check in into git. 
env.start_explorer_in_container()?;
```

this will automatically start the podman container, so you need to have podman installed.
It will destroy the container when Testenv is dropped.

#### Step 2: Access the Web Interface

When the frontend container is started, it will log the URL to access the explorer
(via `tracing`, so make sure logging is not switched off with `RUST_LOG=off`). The port always changes and therefore
parallel testing works, even though it makes little sense to have more than one frontend container running.
`env.debug_tx(txid)` logs the explorer URL of a specific transaction.

> **Note**: Keep the Rust environment running while using the web interface. So you need to set a breakpoint,
> then you have time to inspect the blockchain. If the program terminates, the blockchain
> is dropped (and the container).

### Custom Configuration Usage

```rust,ignore
// Create custom configuration
let mut config = Config::default();

// Customize bitcoind settings
config.bitcoind.args.push("-maxmempool=100");

// Wait longer in the wait_for_* methods
config.timeout = Duration::from_secs(10);

// Customize electrsd settings
// config.electrsd.view_stderr = true;  // Uncomment to see electrsd logs

// Create environment with custom configuration
let mut env = TestEnv::new_with_conf(config)?;

env.mine_blocks(5)?;
```

`TestEnvBuilder` offers a shortcut for a fixed RPC password and a persistent data directory:

```rust,ignore
let env = TestEnvBuilder::new(Some("bitcoin".to_owned()))
    .with_data_dir(Some(PathBuf::from("/path/to/data")))
    .build()?;
```

### Environment Variables

```bash
# Override executables via environment variables
export BITCOIND_EXEC="/custom/path/to/bitcoind"
export ELECTRS_EXEC="/custom/path/to/electrs"

cargo test  # Will use custom executables

# Allow several TestEnv instances to run at the same time (faster, but may be flaky).
# By default a global lock serializes them.
TEST_MULTITHREADED=true cargo test
```

### Standalone server

```bash
cargo run --bin testenv-server -p testenv                            # temporary data, deleted on exit
cargo run --bin testenv-server -p testenv -- --data-dir /path/to/dir # persistent data
```

It prints the connection details as `KEY=value` lines (`TESTENV_RPC_URL`, `TESTENV_RPC_USER`, `TESTENV_RPC_PASS`,
`TESTENV_ELECTRUM_URL`, `TESTENV_P2P_ADDR`, ...) and runs until stopped with Ctrl+C. The RPC credentials are
`bitcoin`/`bitcoin`.

## API Reference

### TestEnv

The main environment manager that handles both bitcoind and electrs instances.

> **⚠️ Important**: Keep the `TestEnv` instance alive (don't drop it) while you need the services running. When the instance is dropped, both bitcoind and electrs processes will be terminated.

#### Creation Methods

- `TestEnv::new()` - Creates environment with automatic downloads
- `TestEnv::new_with_conf(config)` - Creates environment with custom configuration
- `TestEnvBuilder` - Builder for a fixed RPC password and/or persistent data directory

#### Client Access

- `electrum_client()` / `bdk_electrum_client()` - Access to the electrum client for blockchain operations
- `new_client()` - Create a new, independent `BdkElectrumClient`
- `new_testchain()` - Create a `Testchain` (implements `ChainApi` / `ChainScanner`)
- `bitcoind_client()` / `bitcoin_core_rpc_client()` - Access to bitcoind's RPC
- `electrum_url()` - Get electrum server URL
- `esplora_url()` - Get Esplora REST URL (if available)
- `bitcoin_rpc_port()` / `bitcoin_rpc_password()` - RPC connection details (user is `bitcoin`)
- `p2p_socket_addr()` - bitcoind's P2P address, e.g. for Compact Block Filter syncing

`TestEnv` itself also implements `ChainScanner`.

#### Web UI

- `start_explorer_in_container()` - Starts btc-rpc-explorer in a podman container
- `debug_tx(txid)` - Logs the explorer URL of a transaction

#### Blockchain Operations

- `mine_block()` - Mine a single block
- `mine_blocks(count)` - Mine multiple blocks
- `fund_address(address, amount)` - Send BTC to address
- `fund_from_prv_key(key, amount)` - Send BTC to the P2TR address of a private key
- `broadcast(tx)` - Broadcast a transaction
- `new_address()` - Generate new test address

#### Synchronization

- `wait_for_block()` - Wait for electrum to see new block
- `wait_for_tx(txid)` - Wait for electrum and bitcoind to see transaction

#### Information

- `block_count()` - Get current blockchain height
- `best_block_hash()` - Get current tip block hash
- `genesis_hash()` - Get genesis block hash

### Utility Methods

- `trigger_sync()` - Trigger electrs sync (Unix only)
- `workdir()` - Get working directory path
- `new_temp_path()` - Get a fresh temporary directory that lives as long as the `TestEnv`
- `TestEnv::get_bound_port()` - Get a `TcpListener` bound to a free local port

## Testing

The executables are downloaded automatically at build time, so the tests need no further setup:

```bash
cargo test -p testenv
```

### Platform Notes

- **Linux/macOS**: Supported
- **Windows**: Not currently supported
