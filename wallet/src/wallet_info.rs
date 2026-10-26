//! Transport-agnostic views of wallet state.
//!
//! These types are what [`crate::bmp_wallet::BMPWallet`] hands back to callers that need to
//! *describe* the wallet (a GUI, an RPC layer) rather than *drive* it. They deliberately hold
//! `rust-bitcoin` types rather than protobuf ones, so the `wallet` crate stays free of any
//! dependency on a particular wire format; mapping to protobuf happens in the `rpc` crate.

use bdk_wallet::bitcoin::{Address, Amount, ScriptBuf, Txid};

/// A bitcoin address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PubAddressInfo {
    pub address: Address,
}

/// A single input of a wallet transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TxInputInfo {
    pub prev_out_tx_id: Txid,
    pub prev_out_index: u32,
    pub sequence: u32,
    pub script_sig: ScriptBuf,
    /// Consensus-encoded witness stack, lower-hex. Empty for a non-segwit input.
    pub witness: String,
}

/// A single output of a wallet transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TxOutputInfo {
    pub value: Amount,
    /// `None` when the script doesn't decode to a standard address for the wallet's network.
    pub address: Option<PubAddressInfo>,
    pub script_pubkey: ScriptBuf,
}

/// A wallet-relevant transaction, flattened for display.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TxInfo {
    pub tx_id: Txid,
    pub inputs: Vec<TxInputInfo>,
    pub outputs: Vec<TxOutputInfo>,
    pub lock_time: u32,
    /// Height of the confirming block, or `None` while the tx is still unconfirmed.
    pub block_height: Option<u32>,
    /// Unix timestamp (**seconds**) of the confirming block; `None` while unconfirmed.
    pub timestamp: Option<u64>,
    /// `0` while unconfirmed, otherwise `tip_height + 1 - confirmation_height`.
    pub num_confirmations: u32,
    /// Net amount this transaction moved, always non-negative. The direction of the movement is
    /// given by [`Self::incoming`], so that the pair can be rendered as a signed value without
    /// the caller having to reason about change outputs.
    pub amount: Amount,
    /// `true` when the wallet received more than it sent, i.e. the net effect was a credit.
    pub incoming: bool,
}

/// An unspent output owned by the wallet (or by one of its imported keys).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UtxoInfo {
    pub tx_id: Txid,
    pub vout: u32,
    pub amount: Amount,
    /// `None` when the script doesn't decode to a standard address for the wallet's network.
    pub address: Option<PubAddressInfo>,
    pub num_confirmations: u32,
    /// `true` when the output is controlled by an imported private key rather than by the
    /// wallet's own HD keychains.
    pub imported: bool,
    pub is_locked: bool,
}
