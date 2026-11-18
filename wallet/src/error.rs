use bdk_wallet::bitcoin::psbt::ExtractTxError;
use thiserror::Error;

use crate::persisted::PersistenceError;

pub type Result<T, E = WalletErrorKind> = std::result::Result<T, E>;

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ChainDataSourceError {
    #[error("chain scanner error: {0}")]
    Scanner(#[from] chain::SyncError),
    #[error("chain scanner returned an update for unknown descriptor {descriptor:?}")]
    UnknownDescriptor { descriptor: bdk_wallet::chain::DescriptorId },
    #[error("chain scanner returned an update for missing wallet index {index}")]
    MissingWallet { index: usize },
    #[error("failed to apply chain update: {0}")]
    ApplyUpdate(#[from] bdk_wallet::chain::local_chain::CannotConnectError),
    #[error("full scan failed: {0}")]
    FullScan(#[from] chain::FullScanError),
}

#[derive(Error, Debug)]
#[error(transparent)]
#[non_exhaustive]
pub enum WalletErrorKind {
    Miniscript(#[from] bdk_wallet::miniscript::Error),
    Persistence(#[from] PersistenceError),
    #[error("failed to derive database key: {0}")]
    KeyDerivation(#[from] argon2::Error),
    #[error("an imported key with this internal key already exists")]
    DuplicateKey,
    #[error("Invalid Password provided")]
    InvalidPassword,
    Signer(#[from] bdk_wallet::signer::SignerError),
    #[error("not a Taproot address")]
    NotTaprootAddress,
    #[error("malformed PSBT")]
    MalformedPsbt,
    Conversion(#[from] bdk_wallet::miniscript::descriptor::ConversionError),
    CreateTx(#[from] bdk_wallet::error::CreateTxError),
    #[error("imported key descriptor does not match its secret key")]
    MismatchedDescriptor,
    #[error("no wallet loaded")]
    NoWallet,
    #[error("wallet database already exists; refusing to overwrite it")]
    WalletAlreadyExists,
    #[error("seed-derived descriptors do not match the persisted wallet descriptors")]
    SeedDescriptorMismatch,
    #[error("UTXO is spent or invalid")]
    InvalidUTXO,
    ExtractTx(#[from] Box<ExtractTxError>),
    LoadWallet(#[from] bdk_wallet::LoadWithPersistError<rusqlite::Error>),
    CreateWallet(#[from] bdk_wallet::CreateWithPersistError<rusqlite::Error>),
    Bip32(#[from] bdk_wallet::bitcoin::bip32::Error),
    Descriptor(#[from] bdk_wallet::descriptor::DescriptorError),
    Mnemonic(#[from] bdk_wallet::keys::bip39::Error),
    Electrum(#[from] bdk_electrum::electrum_client::Error),
    ApplyUpdate(#[from] bdk_wallet::chain::local_chain::CannotConnectError),
    ChainDataSource(#[from] ChainDataSourceError),
    Generic(Box<dyn std::error::Error + Sync + Send + 'static>),
}

impl WalletErrorKind {
    pub fn generic<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Generic(Box::new(err))
    }
}

impl From<ExtractTxError> for WalletErrorKind {
    fn from(value: ExtractTxError) -> Self {
        Self::ExtractTx(Box::new(value))
    }
}

impl From<rusqlite::Error> for WalletErrorKind {
    fn from(value: rusqlite::Error) -> Self {
        PersistenceError::Database(value).into()
    }
}
