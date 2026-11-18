//! Transaction confidence data of a wallet, and the error type of the wallet sync. Both are used
//! by the `wallet.Wallet` service in [`crate::bmp_wallet_service`].

use std::sync::Arc;

use bdk_wallet::Wallet;
use bdk_wallet::bitcoin::{Transaction, Txid};
use bdk_wallet::chain::{ChainPosition, ConfirmationBlockTime};
use thiserror::Error;
use tracing::trace;

pub(crate) fn tx_confidence_entries(
    wallet: &Wallet,
) -> impl Iterator<Item = (Txid, TxConfidence)> + '_ {
    trace!( "Syncing confirmations.");

    let next_height = wallet.latest_checkpoint().height() + 1;
    wallet.transactions()
        .map(move |wallet_tx| {
            let wallet_tx: WalletTx = wallet_tx.into();
            let conf_height = wallet_tx.chain_position.confirmation_height_upper_bound().unwrap_or(next_height);
            let num_confirmations = next_height - conf_height;
            trace!(%num_confirmations, %wallet_tx.txid, "New transaction confirmations.");
            (wallet_tx.txid, TxConfidence { wallet_tx, num_confirmations })
        })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TxConfidence {
    pub wallet_tx: WalletTx,
    pub num_confirmations: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletTx {
    pub txid: Txid,
    pub tx: Arc<Transaction>,
    pub chain_position: ChainPosition<ConfirmationBlockTime>,
}

impl From<bdk_wallet::WalletTx<'_>> for WalletTx {
    fn from(value: bdk_wallet::WalletTx) -> Self {
        Self { txid: value.tx_node.txid, tx: value.tx_node.tx, chain_position: value.chain_position }
    }
}

pub type Result<T, E = WalletErrorKind> = std::result::Result<T, E>;

#[derive(Error, Debug)]
#[error(transparent)]
#[non_exhaustive]
pub enum WalletErrorKind {
    /// Raised by wallet impls that sync through a `ChainDataSource`, where the underlying failure
    /// is not usefully typed.
    ChainSync(#[from] anyhow::Error),
}
