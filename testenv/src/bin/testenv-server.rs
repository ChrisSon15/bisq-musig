//! Usage:
//!   cargo run --bin testenv-server -- --data-dir /path/to/persistent/dir
//!   cargo run --bin testenv-server  # Uses `TempDir` (auto-deleted on exit)

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use testenv::TestEnvBuilder;

#[derive(Parser, Debug)]
#[command(name = "testenv-server")]
#[command(about = "Bitcoin regtest environment server for integration testing")]
#[command(version)]
struct Args {
    /// Optional persistent data directory. If not provided, uses a temporary directory
    /// that will be auto-deleted when the server stops.
    #[arg(short, long)]
    data_dir: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Install the shutdown handler before starting bitcoind, so that a signal arriving during
    // its start-up still ends in the orderly shutdown below (dropping `env` stops the daemon
    // and removes its temporary directory) rather than in the default signal action, which
    // would leave it running. Handles SIGINT (Ctrl+C), SIGTERM and SIGHUP
    // (the `termination` feature of `ctrlc`).
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let running_clone = running.clone();
    ctrlc::set_handler(move || {
        running_clone.store(false, std::sync::atomic::Ordering::SeqCst);
    })
    .expect("Error setting shutdown signal handler");

    // Create TestEnv with optional data directory
    let env = TestEnvBuilder::new(Some("bitcoin".to_owned()))
        .with_data_dir(args.data_dir.clone())
        .build()?;

    // Output connection information in a parseable format
    let rpc_port = env.bitcoin_rpc_port();
    let rpc_url = format!("http://127.0.0.1:{rpc_port}");
    let workdir = env.workdir();

    // Output as key=value pairs for easy parsing
    println!("TESTENV_READY=true");
    println!("TESTENV_RPC_URL={rpc_url}");
    println!("TESTENV_RPC_PORT={rpc_port}");
    println!("TESTENV_RPC_USER=bitcoin");
    println!("TESTENV_RPC_PASS={}", env.bitcoin_rpc_password());
    // bitcoind's P2P address. Needed by clients that sync over compact block filters (BIP157/158)
    // rather than RPC — testenv already starts bitcoind with -blockfilterindex/-peerblockfilters.
    if let Some(p2p) = env.p2p_socket_addr() {
        println!("TESTENV_P2P_ADDR={p2p}");
    }
    println!("TESTENV_WORKDIR={}", workdir.display());
    if let Some(data_dir) = &args.data_dir {
        println!("TESTENV_PERSISTENT=true");
        println!("TESTENV_DATA_DIR={}", data_dir.display());
    } else {
        println!("TESTENV_PERSISTENT=false");
    }

    eprintln!("TestEnv server started successfully");
    eprintln!("Bitcoin RPC: {rpc_url} (user: bitcoin)");
    eprintln!("Working directory: {}", workdir.display());
    if let Some(data_dir) = &args.data_dir {
        eprintln!("Persistent data directory: {}", data_dir.display());
    }

    eprintln!("Server running. Press Ctrl+C (or send SIGTERM) to stop.");

    // Keep running until a shutdown signal is received
    while running.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    eprintln!("Shutting down TestEnv server...");
    Ok(())
}
