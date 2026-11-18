use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::vec;

use bdk_wallet::bitcoin::bip32::Xpriv;
use bdk_wallet::bitcoin::hex::DisplayHex as _;
use bdk_wallet::bitcoin::{
    Address, Amount, BlockHash, FeeRate, Network, OutPoint, PrivateKey, Psbt, ScriptBuf, Sequence,
    TapNodeHash, Transaction, Txid, Weight, XOnlyPublicKey, consensus, psbt,
};
use bdk_wallet::chain::{
    BlockId, CanonicalizationParams, ChainPosition, CheckPoint, FullTxOut, Merge as _,
};
use bdk_wallet::descriptor::ExtendedDescriptor;
use bdk_wallet::error::CreateTxError;
use bdk_wallet::keys::bip39::Mnemonic;
use bdk_wallet::miniscript::descriptor::{KeyMap, TapTree, Tr};
use bdk_wallet::miniscript::psbt::PsbtExt as _;
use bdk_wallet::rusqlite::Connection;
use bdk_wallet::signer::{InputSigner as _, SignerContext, SignerError, SignerWrapper};
use bdk_wallet::template::{Bip86, DescriptorTemplate as _};
use bdk_wallet::{
    AddressInfo, Balance, KeychainKind, LocalOutput, PersistedWallet, SignOptions, TxBuilder,
    Update, Utxo, Wallet, WeightedUtxo,
};
use rand::RngCore as _;
use zeroize::Zeroizing;

use crate::chain_data_source::ChainDataSource;
use crate::coin_selection::{AlwaysSpendImportedFirst, SpendImportedOnly};
use crate::error::{Result, WalletErrorKind};
use crate::key_shares::{TradeKeyShares, derive_trade_key_shares};
use crate::password::Password;
use crate::persisted::{BMPDatabase, BMPWalletPersister as _, DBStorage, PersistenceError};
use crate::protocol_wallet_api::{
    ProtocolWalletApi, WalletExt, finish_standard_psbt, internal_key_at_index, seconds_since_epoch,
    sign_selected_inputs_with,
};
use crate::secret::SecretScalar;
use crate::utils::{derive_key_from_password, key_verifier};
use crate::wallet_info::{PubAddressInfo, TxInfo, TxInputInfo, TxOutputInfo, UtxoInfo};
/// An external (non-HD) private key imported into the wallet, together with the Taproot output
/// template it controls: `tr(P, tap_tree)` where `P` is the (untweaked) internal key derived from
/// `secret`. A missing tap tree means a key-path-only output (`tr(P)`, as in BIP86).
///
/// The full descriptor is kept (rather than just the merkle root) because BDK wallets are
/// descriptor-driven: it is what lets the per-key sub-wallet in [`load_imported_wallets`] watch
/// the right script pubkey, and what [`BMPWallet::imported_utxos`] and [`WalletApi::sign`] use
/// to recognise and tweak-sign the output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedKey {
    secret: SecretScalar,
    descriptor: Tr<XOnlyPublicKey>,
}

impl ImportedKey {
    pub fn new(
        secret: impl Into<SecretScalar>,
        tap_tree: Option<TapTree<XOnlyPublicKey>>,
    ) -> Result<Self> {
        let secret = secret.into();
        let descriptor = Tr::new(Self::internal_key_of(&secret), tap_tree)?;
        Ok(Self { secret, descriptor })
    }

    /// Rebuild from the persisted `tr(..)` descriptor string; checks that it belongs to `secret`.
    pub(crate) fn from_descriptor_str(secret: SecretScalar, descriptor: &str) -> Result<Self> {
        let descriptor: Tr<XOnlyPublicKey> = descriptor.parse()?;
        if *descriptor.internal_key() != Self::internal_key_of(&secret) {
            return Err(WalletErrorKind::MismatchedDescriptor);
        }
        Ok(Self { secret, descriptor })
    }

    fn internal_key_of(secret: &SecretScalar) -> XOnlyPublicKey {
        XOnlyPublicKey::from_slice(&secret.public_point().serialize_xonly())
            .expect("Should be valid xonly pubkey")
    }

    /// The secret key. It stays in memory as long as the wallet is open, because the wallet
    /// signs with it, and it is wiped when the key is dropped.
    pub const fn secret(&self) -> &SecretScalar {
        &self.secret
    }

    pub fn internal_key(&self) -> XOnlyPublicKey {
        *self.descriptor.internal_key()
    }

    pub fn tap_tree(&self) -> Option<&TapTree<XOnlyPublicKey>> {
        self.descriptor.tap_tree().as_ref()
    }

    pub const fn descriptor(&self) -> &Tr<XOnlyPublicKey> {
        &self.descriptor
    }

    pub fn merkle_root(&self) -> Option<TapNodeHash> {
        self.descriptor.spend_info().merkle_root()
    }

    pub fn script_pubkey(&self) -> ScriptBuf {
        self.descriptor.script_pubkey()
    }
}

pub(crate) const STOP_GAP: usize = 50;

/// The number of blocks of the main wallet that the wallet of a newly imported key starts with.
/// A sync with compact block filters steps back 7 known blocks from the tip of each wallet, to be
/// safe against a chain reorganisation, so the wallet needs more known blocks than that.
const START_CHECKPOINT_COUNT: usize = 10;

pub(crate) struct ImportedWallet {
    pub(crate) wallet: PersistedWallet<Connection>,
    db_name: String,
    #[cfg(any(test, feature = "test-utils"))]
    db: Option<Connection>,
}

impl ImportedWallet {
    fn persist(&mut self, storage: &DBStorage, db_key: &str) -> Result<bool> {
        if self.wallet.staged().is_none() {
            return Ok(false);
        }

        #[cfg(any(test, feature = "test-utils"))]
        if let Some(db) = self.db.as_mut() {
            return Ok(self.wallet.persist(db)?);
        }

        let mut db = storage
            .sibling(&self.db_name)
            .open_with_raw_key(&self.db_name, db_key)?;
        Ok(self.wallet.persist(&mut db)?)
    }
}

pub struct BMPWallet {
    wallet: PersistedWallet<Connection>,
    imported_keys: HashMap<ScriptBuf, ImportedKey>,
    imported_wallets: Vec<ImportedWallet>,
    signers_loaded: bool,
    db: BMPDatabase<Connection>,
    /// The key that encrypts the database files of the imported keys. It is random, stored in
    /// the main database, and independent of the password, so a password change does not touch
    /// these files. It stays in memory while the wallet is open, because the files are opened
    /// for every write.
    imported_db_key: Zeroizing<String>,
    last_unused_address: Option<String>,
    /// Argon2 salt backing the current database key, mirrored on disk as `<db_path>.salt`.
    salt: Vec<u8>,
    /// SHA-256 fingerprint of the `SQLCipher` key currently in force. Retained so a caller-
    /// supplied password can be *verified* (re-derive, fingerprint, compare) without keeping the
    /// plaintext password — or the key itself — in memory: the derived key only exists
    /// transiently, for `PRAGMA key`/`rekey`, and is zeroized right after use.
    key_verifier: [u8; 32],
}

struct Bip86Keys {
    external: (ExtendedDescriptor, KeyMap),
    internal: (ExtendedDescriptor, KeyMap),
}

impl BMPWallet {
    fn bip86_keys_from_seed_phrase(network: Network, mnemonic: &Mnemonic) -> Result<Bip86Keys> {
        let seed = Zeroizing::new(mnemonic.to_seed(""));
        let xprv = Xpriv::new_master(network, &seed[..])?;

        let (external, external_map, _) =
            Bip86(xprv, KeychainKind::External).build(network.into())?;
        let (internal, internal_map, _) =
            Bip86(xprv, KeychainKind::Internal).build(network.into())?;

        Ok(Bip86Keys {
            external: (external, external_map),
            internal: (internal, internal_map),
        })
    }

    fn create_wallet_from_seed(
        db: &mut Connection,
        network: Network,
        mnemonic: &Mnemonic,
    ) -> Result<PersistedWallet<Connection>> {
        let Bip86Keys {
            external: (descriptor, external_map),
            internal: (change_descriptor, internal_map),
        } = Self::bip86_keys_from_seed_phrase(network, mnemonic)?;

        Ok(Wallet::create(descriptor, change_descriptor)
            .network(network)
            .keymap(KeychainKind::External, external_map)
            .keymap(KeychainKind::Internal, internal_map)
            .create_wallet(db)?)
    }

    /// Reads the persisted seed phrase and parses it as BIP39.
    fn load_mnemonic(db: &Connection) -> Result<Mnemonic> {
        let text = Zeroizing::new(Connection::get_seed_phrase(db, Self::SEEDS_TABLE_NAME)?);
        Ok(Mnemonic::parse_normalized(&text)?)
    }

    pub fn list_unused_addresses_since_last_used(
        &self,
        key_chain: KeychainKind,
    ) -> impl Iterator<Item = AddressInfo> + '_ {
        let last_used = self.spk_index().last_used_index(key_chain);
        self.list_unused_addresses(key_chain)
            .filter(move |info| last_used.is_none_or(|idx| info.index > idx))
    }

    pub fn next_address(&mut self, key_chain: KeychainKind) -> Result<AddressInfo> {
        let unused = self
            .list_unused_addresses_since_last_used(key_chain)
            .collect::<Vec<_>>();

        let addr = if unused.len() >= STOP_GAP {
            // Find the position of the last returned address, or start at the beginning
            let next_index = if let Some(last_addr) = &self.last_unused_address {
                // Search for the last address in the current unused list
                unused
                    .iter()
                    .position(|info| info.address.to_string() == *last_addr)
                    .map_or(0, |idx| (idx + 1) % unused.len())
            } else {
                // No previous address, start with the first one
                0
            };

            let selected = unused[next_index].clone();

            // Update index to track the address just given out
            self.last_unused_address = Some(selected.address.to_string());

            selected
        } else {
            let addr = self.reveal_next_address(key_chain);
            self.persist()?;
            // Reset the index since we've generated new addresses
            self.last_unused_address = None;
            addr
        };

        Ok(addr)
    }

    /// Reveals a new address of the external keychain for a trade.
    ///
    /// A key that was issued to a trade is never issued again, neither to another trade nor as
    /// a receive address of the user: every pre-signed transaction of the trade pays to it or
    /// uses it in a script, and the transactions stay valid after the trade. The index is stored
    /// before the address leaves the wallet, and the key counts as used from then on, so
    /// [`Self::next_address`] does not return it.
    fn new_trade_address(&mut self) -> Result<AddressInfo> {
        let address = self.wallet.reveal_next_address(KeychainKind::External);
        Connection::persist_trade_key_index(
            &mut self.db,
            Self::TRADE_KEYS_TABLE_NAME,
            address.index,
        )?;
        self.wallet.mark_used(KeychainKind::External, address.index);
        self.persist()?;
        Ok(address)
    }

    /// Import an external private key (i.e. one not derived from the HD wallet) controlling a
    /// `tr(P, tap_tree)` output, `P` being the untweaked internal key of `pk`. `None` means a
    /// key-path-only `tr(P)` output. After importing, a rescan should be triggered.
    pub fn import_private_key(
        &mut self,
        pk: impl Into<SecretScalar>,
        tap_tree: Option<TapTree<XOnlyPublicKey>>,
    ) -> Result<()> {
        self.import_key(pk.into(), tap_tree, None)
    }

    /// Imports a key like [`Self::import_private_key`], for a key that has no confirmed output
    /// at or below the height `start_height`.
    ///
    /// The wallet of the new key starts with the blocks of the main wallet up to this height.
    /// Without them it would know only the genesis block, and the next sync with compact block
    /// filters would start there for all wallets, because the sync starts at the lowest block
    /// that any of the wallets knows.
    pub fn import_private_key_since(
        &mut self,
        pk: impl Into<SecretScalar>,
        tap_tree: Option<TapTree<XOnlyPublicKey>>,
        start_height: u32,
    ) -> Result<()> {
        self.import_key(pk.into(), tap_tree, Some(start_height))
    }

    fn import_key(
        &mut self,
        pk: SecretScalar,
        tap_tree: Option<TapTree<XOnlyPublicKey>>,
        start_height: Option<u32>,
    ) -> Result<()> {
        let key_to_import = ImportedKey::new(pk, tap_tree)?;
        if Self::is_duplicate_imported_key(&self.imported_keys, &key_to_import) {
            return Err(WalletErrorKind::DuplicateKey);
        }

        let mut imported_wallet = Self::load_imported_wallet(
            &key_to_import,
            self.db.storage(),
            self.network(),
            &self.imported_db_key,
        )?;
        // Only a wallet that knows nothing but the genesis block gets a start: a wallet that was
        // loaded from an earlier import of the same key has seen the chain already.
        if imported_wallet.wallet.latest_checkpoint().height() == 0
            && let Some(start) = start_height.and_then(|height| self.checkpoints_up_to(height))
        {
            imported_wallet.wallet.apply_update(Update {
                chain: Some(start),
                ..Update::default()
            })?;
        }
        self.imported_keys
            .insert(key_to_import.script_pubkey(), key_to_import);
        self.imported_wallets.push(imported_wallet);
        self.persist()?;
        Ok(())
    }

    /// The blocks that the main wallet knows at or below `height`: the genesis block and the
    /// highest [`START_CHECKPOINT_COUNT`] of them. `None` if the main wallet knows no such block
    /// above the genesis block.
    fn checkpoints_up_to(&self, height: u32) -> Option<CheckPoint> {
        let mut blocks = self
            .wallet
            .checkpoints()
            .filter(|checkpoint| (1..=height).contains(&checkpoint.height()))
            .take(START_CHECKPOINT_COUNT)
            .map(|checkpoint| checkpoint.block_id())
            .collect::<Vec<_>>();
        if blocks.is_empty() {
            return None;
        }
        blocks.push(BlockId {
            height: 0,
            hash: self.wallet.local_chain().genesis_hash(),
        });
        blocks.reverse();
        CheckPoint::from_block_ids(blocks).ok()
    }

    /// Test-only introspection of the imported keys; enable the `test-utils` feature to use it
    /// from tests in downstream crates.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn imported_keys(&self) -> Vec<ImportedKey> {
        let mut keys = self.imported_keys.values().cloned().collect::<Vec<_>>();
        keys.sort_by(|left, right| {
            left.script_pubkey()
                .as_bytes()
                .cmp(right.script_pubkey().as_bytes())
        });
        keys
    }

    /// The heights of the chain tips that the wallets of the imported keys know, in the order in
    /// which the wallets were loaded.
    pub fn imported_wallet_tip_heights(&self) -> Vec<u32> {
        self.imported_wallets
            .iter()
            .map(|imported| imported.wallet.latest_checkpoint().height())
            .collect()
    }

    pub fn imported_balance(&self) -> Balance {
        self.imported_wallets
            .iter()
            .fold(Balance::default(), |balance, imported| {
                balance + imported.wallet.balance()
            })
    }

    /// The aggregate balance of currently unspent locked outputs.
    pub fn locked_balance(&self) -> Balance {
        self.balance_for_lock_state(true) + self.imported_balance_for_lock_state(true)
    }

    /// The balance available to a newly-built transaction.
    ///
    /// `full_balance` remains the complete balance breakdown, including the reserved balance of
    /// locked outputs.
    pub fn available_balance(&self) -> Balance {
        self.balance_for_lock_state(false) + self.imported_balance_for_lock_state(false)
    }

    fn wallet_balance_for_lock_state(
        wallet: &PersistedWallet<Connection>,
        locked: bool,
        is_locked: impl Fn(OutPoint) -> bool,
    ) -> Balance {
        let outpoints = wallet
            .spk_index()
            .outpoints()
            .iter()
            .copied()
            .filter(|(_, outpoint)| is_locked(*outpoint) == locked);
        wallet.tx_graph().balance(
            wallet.local_chain(),
            wallet.latest_checkpoint().block_id(),
            CanonicalizationParams::default(),
            outpoints,
            |(keychain, _), _| *keychain == KeychainKind::Internal,
        )
    }

    fn balance_for_lock_state(&self, locked: bool) -> Balance {
        Self::wallet_balance_for_lock_state(&self.wallet, locked, |outpoint| {
            self.is_outpoint_locked(outpoint)
        })
    }

    fn imported_balance_for_lock_state(&self, locked: bool) -> Balance {
        self.imported_wallets
            .iter()
            .fold(Balance::default(), |balance, imported| {
                balance
                    + Self::wallet_balance_for_lock_state(&imported.wallet, locked, |outpoint| {
                        self.is_outpoint_locked(outpoint)
                    })
            })
    }

    fn imported_unspent(
        &self,
    ) -> impl Iterator<Item = (&PersistedWallet<Connection>, LocalOutput)> + '_ {
        self.imported_wallets.iter().flat_map(|imported_wallet| {
            imported_wallet
                .wallet
                .list_unspent()
                .map(move |local| (&imported_wallet.wallet, local))
            })
    }

    fn imported_utxo_is_immature(
        wallet: &PersistedWallet<Connection>,
        local: &LocalOutput,
    ) -> bool {
        let full_txo = FullTxOut {
            chain_position: local.chain_position,
            outpoint: local.outpoint,
            txout: local.txout.clone(),
            spent_by: None,
            is_on_coinbase: wallet
                .tx_graph()
                .get_tx(local.outpoint.txid)
                .is_some_and(|tx| tx.is_coinbase()),
        };
        !full_txo.is_mature(wallet.latest_checkpoint().height())
    }

    fn imported_utxos(&self) -> Vec<WeightedUtxo> {
        self.imported_unspent()
            .filter_map(|(wallet, local)| {
                if self.is_utxo_locked(local.outpoint)
                    || Self::imported_utxo_is_immature(wallet, &local)
                {
                    return None;
                }

                let output_script_pubkey = &local.txout.script_pubkey;
                let (tap_internal_key, tap_merkle_root) = self
                    .imported_keys
                    .get(output_script_pubkey)
                    .map_or((None, None), |key| {
                        (Some(key.internal_key()), key.merkle_root())
                    });

                Some(WeightedUtxo {
                    utxo: Utxo::Foreign {
                        outpoint: local.outpoint,
                        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                        psbt_input: Box::new(psbt::Input {
                            witness_utxo: Some(local.txout),
                            tap_internal_key,
                            tap_merkle_root,
                            ..Default::default()
                        }),
                    },
                    // The witness of a Taproot key spend with the default signature hash
                    // type: 1 byte for the number of items, 1 byte for the length, and the
                    // signature of 64 bytes. A smaller value makes the transaction pay less
                    // than the fee rate that the caller asked for.
                    satisfaction_weight: Weight::from_wu_usize(66),
                })
            })
            .collect()
    }

    fn build_tx(&mut self) -> TxBuilder<'_, AlwaysSpendImportedFirst> {
        let imported_weighted_utxos = self.imported_utxos();
        let coin_selection = AlwaysSpendImportedFirst(imported_weighted_utxos);
        self.wallet.build_tx().coin_selection(coin_selection)
    }

    /// The full balance breakdown, combining the HD wallet with any imported keys.
    ///
    /// [`WalletApi::balance`] flattens this to a single spendable [`Amount`]; callers that need
    /// to distinguish confirmed from pending funds should use this instead.
    pub fn full_balance(&self) -> Balance {
        self.imported_balance() + self.wallet.balance()
    }

    /// Every address revealed so far on either keychain, external first.
    pub fn list_wallet_addresses(&self) -> Vec<PubAddressInfo> {
        [KeychainKind::External, KeychainKind::Internal]
            .into_iter()
            .flat_map(|kind| {
                // `derivation_index` is `None` until the keychain has revealed anything, in
                // which case the inner range is skipped entirely.
                self.wallet
                    .derivation_index(kind)
                    .into_iter()
                    .flat_map(move |last_revealed| {
                        (0..=last_revealed).map(move |index| PubAddressInfo {
                            address: self.wallet.peek_address(kind, index).address,
                        })
                    })
            })
            .collect()
    }

    /// The imported key controlling `script_pubkey`, if any.
    fn imported_key_for_script(&self, script_pubkey: &ScriptBuf) -> Option<&ImportedKey> {
        self.imported_keys.get(script_pubkey)
    }

    fn is_duplicate_imported_key(
        imported_keys: &HashMap<ScriptBuf, ImportedKey>,
        key: &ImportedKey,
    ) -> bool {
        imported_keys.contains_key(&key.script_pubkey())
            || imported_keys
                .values()
                .any(|existing| existing.internal_key() == key.internal_key())
    }

    fn insert_imported_key(
        imported_keys: &mut HashMap<ScriptBuf, ImportedKey>,
        key: ImportedKey,
    ) -> Result<()> {
        if Self::is_duplicate_imported_key(imported_keys, &key) {
            return Err(WalletErrorKind::DuplicateKey);
        }

        imported_keys.insert(key.script_pubkey(), key);
        Ok(())
    }

    /// Renders `script_pubkey` as an address, or `None` if it isn't a standard one.
    fn address_for_script(&self, script_pubkey: &ScriptBuf) -> Option<PubAddressInfo> {
        Address::from_script(script_pubkey, self.wallet.network())
            .ok()
            .map(|address| PubAddressInfo { address })
    }

    /// Confirmation count for a chain position: `0` while unconfirmed.
    fn num_confirmations_for(
        wallet: &PersistedWallet<Connection>,
        block_height: Option<u32>,
    ) -> u32 {
        let next_height = wallet.latest_checkpoint().height() + 1;
        block_height.map_or(0, |height| next_height.saturating_sub(height))
    }

    fn num_confirmations(&self, block_height: Option<u32>) -> u32 {
        Self::num_confirmations_for(&self.wallet, block_height)
    }

    /// All unspent outputs, both the HD wallet's own and those held by imported keys.
    ///
    /// Imported outputs come directly from the corresponding imported wallets. Their `TxOut`s are
    /// also kept in the main graph so fee calculation can inspect transactions spending them.
    pub fn list_utxos(&self) -> Vec<UtxoInfo> {
        let own = self.list_unspent().map(|utxo| {
            let block_height = match utxo.chain_position {
                ChainPosition::Confirmed { anchor, .. } => Some(anchor.block_id.height),
                ChainPosition::Unconfirmed { .. } => None,
            };
            UtxoInfo {
                tx_id: utxo.outpoint.txid,
                vout: utxo.outpoint.vout,
                amount: utxo.txout.value,
                address: self.address_for_script(&utxo.txout.script_pubkey),
                num_confirmations: self.num_confirmations(block_height),
                imported: false,
                is_locked: self.is_outpoint_locked(utxo.outpoint),
            }
        });

        let imported = self
            .imported_unspent()
            .filter_map(|(wallet, local)| {
                let outpoint = local.outpoint;
                let block_height = match local.chain_position {
                    ChainPosition::Confirmed { anchor, .. } => Some(anchor.block_id.height),
                    ChainPosition::Unconfirmed { .. } => None,
                };
                self.imported_key_for_script(&local.txout.script_pubkey)
                    .map(|_| UtxoInfo {
                        tx_id: outpoint.txid,
                        vout: outpoint.vout,
                        amount: local.txout.value,
                        address: self.address_for_script(&local.txout.script_pubkey),
                        num_confirmations: Self::num_confirmations_for(wallet, block_height),
                        imported: true,
                        is_locked: self.is_outpoint_locked(outpoint),
                    })
            });

        own.chain(imported).collect()
    }

    /// Every wallet-relevant transaction, flattened for display.
    pub fn list_transactions(&self) -> Vec<TxInfo> {
        self.wallet
            .transactions()
            .map(|wallet_tx| {
                let tx = wallet_tx.tx_node.tx.as_ref();
                let (sent, received) = self.wallet.sent_and_received(tx);
                // `sent` counts every input we own and `received` every output we own (change
                // included), so the difference is the true net effect on the wallet.
                let incoming = received >= sent;
                let amount = if incoming {
                    received - sent
                } else {
                    sent - received
                };

                let (block_height, timestamp) = match &wallet_tx.chain_position {
                    ChainPosition::Confirmed { anchor, .. } => {
                        (Some(anchor.block_id.height), Some(anchor.confirmation_time))
                    }
                    ChainPosition::Unconfirmed { .. } => (None, None),
                };

                TxInfo {
                    tx_id: wallet_tx.tx_node.txid,
                    inputs: tx
                        .input
                        .iter()
                        .map(|txin| TxInputInfo {
                            prev_out_tx_id: txin.previous_output.txid,
                            prev_out_index: txin.previous_output.vout,
                            sequence: txin.sequence.to_consensus_u32(),
                            script_sig: txin.script_sig.clone(),
                            witness: consensus::serialize(&txin.witness).to_lower_hex_string(),
                        })
                        .collect(),
                    outputs: tx
                        .output
                        .iter()
                        .map(|txout| TxOutputInfo {
                            value: txout.value,
                            address: self.address_for_script(&txout.script_pubkey),
                            script_pubkey: txout.script_pubkey.clone(),
                        })
                        .collect(),
                    lock_time: tx.lock_time.to_consensus_u32(),
                    block_height,
                    timestamp,
                    num_confirmations: self.num_confirmations(block_height),
                    amount,
                    incoming,
                }
            })
            .collect()
    }

    /// Checks `password` against the key currently protecting the database, without needing the
    /// plaintext password (or the key itself) to have been retained: the freshly derived key is
    /// compared by its one-way fingerprint and dropped again.
    pub fn check_password(&self, password: &Password) -> Result<bool> {
        let key = derive_key_from_password(password, &self.salt)?;
        Ok(key_verifier(&key) == self.key_verifier)
    }

    /// Re-keys the database to `new_password`, rotating the Argon2 salt at the same time.
    ///
    /// The rotation is staged so that the database is at no point keyed by a salt that exists
    /// nowhere on disk (a crash then would make the wallet permanently unopenable): the new salt
    /// is written to `<db_path>.salt.new` *before* the re-key, and renamed over `<db_path>.salt`
    /// after it. A crash in between is healed on the next [`WalletApi::load_wallet`], which
    /// falls back to the staged salt and completes the rename.
    ///
    /// This is the raw primitive and authenticates nobody: `old_password` is only used to undo
    /// the re-key when committing the new salt fails. Every caller must verify it first — see
    /// [`Self::change_password`].
    fn rekey(&mut self, old_password: &Password, new_password: &Password) -> Result<()> {
        let mut salt = [0u8; 16];
        rand::rng().fill_bytes(&mut salt);
        let new_key = derive_key_from_password(new_password, &salt)?;

        self.db.storage().write_staged_salt(Self::DB_NAME, &salt)?;

        if let Err(e) = self.db.pragma_update(None, "rekey", new_key.as_str()) {
            self.db.storage().remove_staged_salt(Self::DB_NAME);
            return Err(PersistenceError::Database(e).into());
        }

        // Commit the rotated salt. If that fails, undo the re-key with a key re-derived from
        // the (just verified) old password, rather than leave database and salt out of step.
        if let Err(e) = self.db.storage().commit_staged_salt(Self::DB_NAME) {
            let old_key = derive_key_from_password(old_password, &self.salt)?;
            self.db.pragma_update(None, "rekey", old_key.as_str())?;
            self.db.storage().remove_staged_salt(Self::DB_NAME);
            return Err(e.into());
        }

        self.salt = salt.to_vec();
        self.key_verifier = key_verifier(&new_key);
        Ok(())
    }

    /// Changes the wallet password from `old_password` to `new_password`, re-keying the
    /// database and rotating the Argon2 salt.
    ///
    /// `old_password` must be the password currently in force, so nobody who cannot present it
    /// can rotate the key and lock the owner out. The wallet is always password-protected: a
    /// password can only be replaced, never removed, and `new_password` follows the password rules
    /// by construction (see [`Password`]).
    pub fn change_password(
        &mut self,
        old_password: &Password,
        new_password: &Password,
    ) -> Result<()> {
        if !self.check_password(old_password)? {
            return Err(WalletErrorKind::InvalidPassword);
        }
        self.rekey(old_password, new_password)
    }

    /// Builds, signs and persists a payment to `address`. The caller is responsible for
    /// broadcasting the returned transaction.
    ///
    /// The signed transaction is recorded in the wallet as unconfirmed before it is returned,
    /// so its inputs count as spent from here on. The wallet cannot rely on the chain to tell
    /// it: compact block filter sync only ever reports confirmed transactions, so until the
    /// payment was mined the spent coins would still look unspent, and a second payment would
    /// happily pick the same inputs — and, since BDK opts into RBF, could replace the first one
    /// in the mempool. Treating a signed-and-handed-out transaction as spent is the safe
    /// direction: a payment that never made it to the network leaves coins looking locked
    /// (recoverable) rather than double-spent (not).
    pub fn send_to_address(
        &mut self,
        address: &Address,
        amount: Amount,
        fee_rate: FeeRate,
    ) -> Result<Transaction> {
        let mut builder = self.build_tx();
        builder
            .fee_rate(fee_rate)
            .add_recipient(address.script_pubkey(), amount);
        let mut psbt = builder.finish()?;

        <Self as WalletApi>::sign(self, &mut psbt, SignOptions::default())?;
        let tx = psbt.extract_tx()?;
        self.record_unconfirmed_transaction(&tx)?;

        Ok(tx)
    }

    /// Moves all coins of imported keys to a new address of the seed wallet: builds the drain
    /// transaction, signs it, records it as unconfirmed and stores it. The caller is responsible
    /// for sending the returned transaction to the network.
    ///
    /// Returns `None`, and changes nothing, if there is no coin to move or if the coins do not
    /// pay for the fee and an output above the dust limit. Locked and immature coins are not
    /// moved.
    ///
    /// The coins of imported keys are payouts of closed trades. The peer of such a trade still
    /// holds a pre-signed transaction that can spend the payout after a lock time, so the coins
    /// are moved before the user stops watching the chain.
    pub fn drain_imported_coins(&mut self, fee_rate: FeeRate) -> Result<Option<Transaction>> {
        if self.imported_utxos().is_empty() {
            return Ok(None);
        }
        let mut psbt = match <Self as WalletApi>::drain_imported_balance(self, fee_rate) {
            Ok(psbt) => psbt,
            Err(WalletErrorKind::CreateTx(
                CreateTxError::CoinSelection(_) | CreateTxError::OutputBelowDustLimit(_),
            )) => return Ok(None),
            Err(error) => return Err(error),
        };
        <Self as WalletApi>::sign(self, &mut psbt, SignOptions::default())?;
        let tx = psbt.extract_tx()?;
        self.record_unconfirmed_transaction(&tx)?;
        Ok(Some(tx))
    }

    /// The unconfirmed transactions that spend a coin of the seed wallet or of an imported key:
    /// payments, drain transactions and transactions of trades that the wallet recorded and that
    /// are in no block yet. The owner of the wallet sends them to the network again until they
    /// are confirmed.
    ///
    /// A transaction that can no longer confirm is not in the list, as far as the wallet knows:
    /// it leaves the list when the wallet sees a confirmed transaction that spends one of the
    /// same coins, or when it is forgotten.
    pub fn unconfirmed_outgoing_transactions(&self) -> Vec<Arc<Transaction>> {
        let mut outgoing: Vec<Arc<Transaction>> = Vec::new();
        let wallets = std::iter::once(&self.wallet).chain(
            self.imported_wallets
                .iter()
                .map(|imported| &imported.wallet),
        );
        for wallet in wallets {
            for tx in wallet.transactions() {
                let (sent, _) = wallet.sent_and_received(&tx.tx_node.tx);
                let known = outgoing
                    .iter()
                    .any(|other| other.compute_txid() == tx.tx_node.txid);
                if !tx.chain_position.is_confirmed() && sent > Amount::ZERO && !known {
                    outgoing.push(Arc::clone(&tx.tx_node.tx));
                }
            }
        }
        outgoing
    }

    /// Records a signed transaction as unconfirmed in the main wallet and in the wallets of the
    /// imported keys, and stores it. Its inputs count as spent from here on.
    pub fn record_unconfirmed_transaction(&mut self, tx: &Transaction) -> Result<()> {
        let last_seen = seconds_since_epoch()?;
        let tx_ref = Arc::new(tx.clone());
        let wallets = std::iter::once(&mut self.wallet).chain(
            self.imported_wallets
                .iter_mut()
                .map(|imported| &mut imported.wallet),
        );
        for wallet in wallets {
            wallet.apply_unconfirmed_txs([(Arc::clone(&tx_ref), last_seen)]);
        }
        <Self as WalletApi>::persist(self)?;
        Ok(())
    }

    fn load_imported_wallet(
        key: &ImportedKey,
        storage: &DBStorage,
        network: Network,
        db_key: &str,
    ) -> Result<ImportedWallet> {
        let db_name = format!("bmp_{}.db3", key.internal_key());
        let imported_storage = storage.sibling(&db_name);
        let mut db = imported_storage.open_with_raw_key(&db_name, db_key)?;
        let descriptor = key.descriptor().to_string();

        let imported_wallet = if let Some(wallet) = Wallet::load()
            .descriptor(KeychainKind::External, Some(descriptor.clone()))
            .check_network(network)
            .extract_keys()
            .load_wallet(&mut db)?
        {
            wallet
        } else {
            Wallet::create_single(descriptor)
                .network(network)
                .create_wallet(&mut db)?
        };

        // BDK's PersistedWallet owns the Wallet state, not the persister connection. Keep the
        // connection only for shared in-memory test databases; file-backed databases reopen it
        // for persist() and therefore do not retain one connection per imported wallet.
        #[cfg(any(test, feature = "test-utils"))]
        let db = storage.base_path().is_none().then_some(db);
        Ok(ImportedWallet {
            wallet: imported_wallet,
            db_name,
            #[cfg(any(test, feature = "test-utils"))]
            db,
        })
    }

    pub(crate) fn load_imported_wallets<'a>(
        imported_keys: impl IntoIterator<Item = &'a ImportedKey>,
        storage: &DBStorage,
        network: Network,
        db_key: &str,
    ) -> Result<Vec<ImportedWallet>> {
        imported_keys
            .into_iter()
            .map(|key| Self::load_imported_wallet(key, storage, network, db_key))
            .collect()
    }

    /// Opens the wallet database at `db_path` with the key derived from `password` and `salt`.
    ///
    /// Factored out of [`WalletApi::load_wallet`] so that it can be retried with the *staged* salt
    /// when recovering from an interrupted password change (see [`BMPWallet::rekey`]).
    fn load_with_salt(
        storage: DBStorage,
        salt: Vec<u8>,
        network: Network,
        password: &Password,
    ) -> Result<Self> {
        let mut db = storage.open(Self::DB_NAME)?;
        let key = derive_key_from_password(password, &salt)?;
        db.pragma_update(None, "key", key.as_str())?;

        let wallet_opt = Wallet::load()
            .check_network(network)
            .load_wallet(&mut db)
            .map_err(|e| match e {
                bdk_wallet::LoadWithPersistError::Persist(rusqlite::Error::SqliteFailure(
                    sqlite_err,
                    _,
                )) if sqlite_err.code == rusqlite::ErrorCode::NotADatabase => {
                    WalletErrorKind::InvalidPassword
                }
                // SQLCipher uses `NotADatabase` for a wrong key; permission failures remain
                // persistence errors so clients do not mistake filesystem access for a password.
                other => other.into(),
            })?;

        let wallet = if let Some(wallet) = wallet_opt {
            wallet
        } else if Connection::has_seed_phrase(&db, Self::SEEDS_TABLE_NAME)? {
            tracing::warn!("No wallet found, but seed phrase exists; creating wallet from seed");
            let mnemonic = Self::load_mnemonic(&db)?;
            Self::create_wallet_from_seed(&mut db, network, &mnemonic)?
        } else {
            return Err(WalletErrorKind::NoWallet);
        };

        let mut imported_keys = HashMap::new();
        for (key_hex, descriptor) in
            Connection::load_imported_key_rows(&mut db, Self::IMPORTED_KEYS_TABLE_NAME)?
        {
            let key_hex = Zeroizing::new(key_hex);
            let key_bytes = Zeroizing::new(
                <Vec<u8> as bdk_wallet::bitcoin::hex::FromHex>::from_hex(&key_hex)
                    .map_err(WalletErrorKind::generic)?,
            );
            let secret = SecretScalar::from_slice(&key_bytes).map_err(WalletErrorKind::generic)?;
            let key = ImportedKey::from_descriptor_str(secret, &descriptor)?;
            Self::insert_imported_key(&mut imported_keys, key)?;
        }

        let imported_db_key = Connection::load_or_create_imported_db_key(&mut db)?;
        let imported_wallets = Self::load_imported_wallets(
            imported_keys.values(),
            &storage,
            network,
            &imported_db_key,
        )?;

        // BDK does not store which keys are marked as used, so the keys that were issued to
        // trades are marked again.
        let mut wallet = wallet;
        for index in Connection::load_trade_key_indexes(&mut db, Self::TRADE_KEYS_TABLE_NAME)? {
            wallet.mark_used(KeychainKind::External, index);
        }

        Ok(Self {
            wallet,
            imported_keys,
            imported_wallets,
            signers_loaded: false,
            db: BMPDatabase::new(storage, db),
            imported_db_key,
            last_unused_address: None,
            salt,
            key_verifier: key_verifier(&key),
        })
    }
}

impl WalletExt for BMPWallet {
    fn update_psbt_with_derivation_paths(&self, psbt: &mut Psbt) {
        self.wallet.update_psbt_with_derivation_paths(psbt);
    }
}

impl ProtocolWalletApi for BMPWallet {
    fn network(&self) -> Network {
        self.wallet.network()
    }

    fn new_address(&mut self) -> Result<Address> {
        Ok(self.new_trade_address()?.address)
    }

    fn new_internal_key(&mut self) -> Result<XOnlyPublicKey> {
        let index = self.new_trade_address()?.index;
        internal_key_at_index(self, index)
    }

    fn create_psbt(
        &mut self,
        recipients: Vec<(ScriptBuf, Amount)>,
        fee_rate: FeeRate,
    ) -> Result<Psbt> {
        finish_standard_psbt(self.build_tx(), recipients, fee_rate)
    }

    fn sign_selected_inputs(
        &mut self,
        psbt: &mut Psbt,
        is_selected: &dyn Fn(&OutPoint) -> bool,
    ) -> Result<()> {
        // TODO unify signing
        sign_selected_inputs_with(self, psbt, is_selected, |w, p, opts| {
            <Self as WalletApi>::sign(w, p, opts)
        })
    }

    fn import_private_key(
        &mut self,
        pk: SecretScalar,
        tap_tree: Option<TapTree<XOnlyPublicKey>>,
    ) -> Result<()> {
        self.import_private_key(pk, tap_tree)
    }

    fn import_private_key_since(
        &mut self,
        pk: SecretScalar,
        tap_tree: Option<TapTree<XOnlyPublicKey>>,
        start_height: u32,
    ) -> Result<()> {
        self.import_private_key_since(pk, tap_tree, start_height)
    }

    fn chain_tip_height(&self) -> u32 {
        self.wallet.latest_checkpoint().height()
    }

    fn record_transaction(&mut self, tx: &Transaction) -> Result<()> {
        self.record_unconfirmed_transaction(tx)
    }

    fn forget_transaction(&mut self, txid: Txid) -> Result<()> {
        let evicted_at = seconds_since_epoch()?;
        let wallets = std::iter::once(&mut self.wallet).chain(
            self.imported_wallets
                .iter_mut()
                .map(|imported| &mut imported.wallet),
        );
        for wallet in wallets {
            wallet.apply_evicted_txs([(txid, evicted_at)]);
        }
        <Self as WalletApi>::persist(self)?;
        Ok(())
    }

    fn trade_key_shares(&self, trade_id: &str) -> Result<TradeKeyShares> {
        let mnemonic = Self::load_mnemonic(&self.db)?;
        let seed = Zeroizing::new(mnemonic.to_seed(""));
        let mut master_key = Xpriv::new_master(self.network(), &seed[..])?;
        let key_shares = derive_trade_key_shares(&master_key, trade_id);
        master_key.private_key.non_secure_erase();
        key_shares
    }

    fn store_trade_record(&mut self, trade_id: &str, record: &[u8]) -> Result<()> {
        // The database commits every statement to disk before it returns.
        Ok(crate::persisted::store_trade_record(
            &self.db, trade_id, record,
        )?)
    }

    fn trade_records(&mut self) -> Result<Vec<(String, Zeroizing<Vec<u8>>)>> {
        Ok(crate::persisted::load_trade_records(&self.db)?)
    }

    fn known_block_at_or_below(&self, height: u32) -> Option<(u32, BlockHash)> {
        self.wallet
            .checkpoints()
            .find(|checkpoint| checkpoint.height() <= height)
            .map(|checkpoint| (checkpoint.height(), checkpoint.hash()))
    }

    fn list_locked_utxos(&self) -> Vec<OutPoint> {
        let mut locked = self.wallet.list_locked_unspent().collect::<Vec<_>>();
        locked.extend(
            self.imported_unspent()
                .filter(|(_, local)| self.is_outpoint_locked(local.outpoint))
                .map(|(_, local)| local.outpoint),
        );
        locked
    }

    fn lock_utxo(&mut self, utxo: OutPoint) -> Result<()> {
        self.lock_outpoint(utxo);
        self.persist()?;
        Ok(())
    }

    fn unlock_utxo(&mut self, utxo: OutPoint) -> Result<()> {
        self.unlock_outpoint(utxo);
        self.persist()?;
        Ok(())
    }

    fn is_utxo_locked(&self, utxo: OutPoint) -> bool {
        self.is_outpoint_locked(utxo)
    }
}

#[trait_variant::make(Send)]
pub trait WalletApi {
    const DB_NAME: &str;
    const SEEDS_TABLE_NAME: &'static str;
    const IMPORTED_KEYS_TABLE_NAME: &'static str;
    const TRADE_KEYS_TABLE_NAME: &'static str;

    /// Creates a fresh wallet in `storage`, protected by `password`.
    fn new(storage: DBStorage, password: &Password, network: Network) -> Result<Self>
    where
        Self: Sized;

    /// Opens the wallet in `storage`; a wrong `password` fails with
    /// [`WalletErrorKind::InvalidPassword`].
    fn load_wallet(storage: DBStorage, network: Network, password: &Password) -> Result<Self>
    where
        Self: Sized;

    fn get_new_address(&mut self) -> Result<AddressInfo>;
    fn get_change_address(&mut self) -> Result<AddressInfo>;

    /// The persisted seed phrase, validated as BIP39; a corrupt stored value is an error.
    ///
    /// The returned [`Mnemonic`] is wiped when dropped. Copies the caller makes from it, such as a
    /// `String` of its words, are not.
    fn get_seed_phrase(&self, password: &Password) -> Result<Mnemonic>;

    fn balance(&self) -> Amount;

    fn persist(&mut self) -> Result<bool>;

    fn build_tx(&mut self) -> TxBuilder<'_, AlwaysSpendImportedFirst>;

    fn sign(&mut self, psbt: &mut Psbt, sign_options: SignOptions) -> Result<()>;

    async fn sync_all(&mut self, s: &(impl ChainDataSource + Sync)) -> Result<()>;

    fn drain_imported_balance(&mut self, fee_rate: FeeRate) -> Result<Psbt>;
}

impl WalletApi for BMPWallet {
    const SEEDS_TABLE_NAME: &'static str = "bmp_seeds";
    const IMPORTED_KEYS_TABLE_NAME: &'static str = "bmp_imported_keys";
    const TRADE_KEYS_TABLE_NAME: &'static str = "bmp_trade_keys";
    const DB_NAME: &str = "bmp_bdk_wallet.db3";

    async fn sync_all(&mut self, s: &(impl ChainDataSource + Sync)) -> Result<()> {
        {
            let mut wallets = vec![&mut self.wallet];
            wallets.extend(
                self.imported_wallets
                    .iter_mut()
                    .map(|imported| &mut imported.wallet),
            );
            s.sync(wallets).await?;
        }

        // Keep the imported TxOuts in the main graph so fee calculation can inspect transactions
        // spending them. Their ownership, chain position, and balance category remain in the
        // imported wallets above.
        for imported in &self.imported_wallets {
            for utxo in imported.wallet.list_unspent() {
                self.wallet.insert_txout(utxo.outpoint, utxo.txout);
            }
        }

        self.persist()?;
        Ok(())
    }

    fn new(storage: DBStorage, password: &Password, network: Network) -> Result<Self>
    where
        Self: Sized,
    {
        // TODO: Make the word size configurable?
        let mut entropy = Zeroizing::new([0u8; 32]);
        rand::rng().fill_bytes(&mut entropy[..]);

        // Never create over an existing wallet.
        if storage.db_exists(Self::DB_NAME) {
            return Err(WalletErrorKind::WalletAlreadyExists);
        }

        let mut db = storage.open(Self::DB_NAME)?;
        let salt = storage.persist_salt(Self::DB_NAME)?;

        let key = derive_key_from_password(password, &salt)?;
        db.pragma_update(None, "key", key.as_str())?;

        Connection::init(
            &mut db,
            Some(Self::IMPORTED_KEYS_TABLE_NAME),
            Some(Self::SEEDS_TABLE_NAME),
        )?;

        let mnemonic = Mnemonic::from_entropy(&entropy[..])?;
        let capacity =
            mnemonic.words().map(str::len).sum::<usize>() + mnemonic.word_count().saturating_sub(1);

        let mut phrase = Zeroizing::new(String::with_capacity(capacity));

        for (i, word) in mnemonic.words().enumerate() {
            if i > 0 {
                phrase.push(' ');
            }
            phrase.push_str(word);
        }

        Connection::persist_seed_phrase(&mut db, Self::SEEDS_TABLE_NAME, &phrase)?;

        let wallet = Self::create_wallet_from_seed(&mut db, network, &mnemonic)?;
        let imported_db_key = Connection::load_or_create_imported_db_key(&mut db)?;

        Ok(Self {
            wallet,
            imported_keys: HashMap::new(),
            imported_wallets: Vec::new(),
            signers_loaded: true,
            db: BMPDatabase::new(storage, db),
            imported_db_key,
            last_unused_address: None,
            salt,
            key_verifier: key_verifier(&key),
        })
    }

    fn persist(&mut self) -> Result<bool> {
        // Persist imported keys and then persist staged changes from ChangeSet
        let imported_keys = self.imported_keys.values().collect::<Vec<_>>();
        Connection::persist_imported_keys(
            &mut self.db,
            Self::IMPORTED_KEYS_TABLE_NAME,
            &imported_keys,
        )?;

        let mut persisted = match self.wallet.staged_mut() {
            Some(stage) => {
                Connection::persist_staged_changes(&mut self.db, &*stage)?;
                let _ = stage.take();
                true
            }
            None => false,
        };

        let storage = self.db.storage().clone();
        for imported in &mut self.imported_wallets {
            persisted |= imported.persist(&storage, &self.imported_db_key)?;
        }

        Ok(persisted)
    }

    fn sign(&mut self, psbt: &mut Psbt, sign_options: SignOptions) -> Result<()> {
        let secp = self.secp_ctx();
        for (input_index, input_details) in psbt.inputs.clone().iter().enumerate() {
            // An input without the output that it spends cannot be recognised as a coin of an
            // imported key. It is left to the signer of the seed wallet below, which signs only
            // what it owns. A PSBT can come from the peer, so it must never stop the process.
            let Some(txout) = input_details.witness_utxo.as_ref() else {
                continue;
            };

            if let Some(signing_key) = self.imported_keys.get(&txout.script_pubkey) {
                // The signer finds its input by the internal key and needs the script tree for
                // the tweak of the key. A PSBT of the trade protocol comes without them: they
                // are removed before the PSBT goes to the peer. The wallet knows both.
                let input = &mut psbt.inputs[input_index];
                input.tap_internal_key = Some(signing_key.internal_key());
                input.tap_merkle_root = signing_key.merkle_root();
                let signer =
                    PrivateKey::from_slice(&signing_key.secret().to_bytes()[..], self.network())
                        .map_err(|_e| SignerError::External("Invalid signing key".to_owned()))?;

                let sw = SignerWrapper::new(
                    signer,
                    SignerContext::Tap {
                        is_internal_key: true,
                    },
                );

                sw.sign_input(psbt, input_index, &sign_options, secp)?;
                psbt.finalize_inp_mut(secp, input_index)
                    .map_err(|_e| SignerError::External("Unable to finalized input".to_owned()))?;
            }
        }

        // Check whether the signing keys were loaded if not load them into the wallet
        if !self.signers_loaded {
            tracing::info!("Loading the signers into the wallet");
            let recovery_phrase = Self::load_mnemonic(&self.db)
                .map_err(|_| SignerError::External("Unable to load keys.".to_owned()))?;
            let Bip86Keys {
                external: (external_descriptor, external_map),
                internal: (internal_descriptor, internal_map),
            } = Self::bip86_keys_from_seed_phrase(self.network(), &recovery_phrase)
                .map_err(|_| SignerError::External("Unable to load keys".to_owned()))?;

            if &external_descriptor != self.wallet.public_descriptor(KeychainKind::External)
                || &internal_descriptor != self.wallet.public_descriptor(KeychainKind::Internal)
            {
                return Err(WalletErrorKind::SeedDescriptorMismatch);
            }

            self.wallet.set_keymap(KeychainKind::External, external_map);
            self.wallet.set_keymap(KeychainKind::Internal, internal_map);
            self.signers_loaded = true;
        }

        // BDK returns `true` only when every input of the PSBT got finalized. Partial-sign use
        // cases (e.g. the trade protocol's half-deposit PSBTs that also carry the peer's still-
        // unsigned inputs) legitimately leave inputs un-finalized, so we don't assert here.
        let _finalized = self.wallet.sign(psbt, sign_options)?;

        Ok(())
    }

    // For already created wallets this will load stored data
    // This will also load the imported keys
    fn load_wallet(storage: DBStorage, network: Network, password: &Password) -> Result<Self> {
        if let Some(p) = storage.base_path() {
            tracing::debug!(path = %p.display(), "Loading wallet database.");
        } else {
            tracing::debug!("Loading in-memory wallet database.");
        }

        let salt = storage.load_salt(Self::DB_NAME)?;
        match Self::load_with_salt(storage.clone(), salt, network, password) {
            Ok(wallet) => {
                storage.remove_staged_salt(Self::DB_NAME);
                Ok(wallet)
            }
            Err(primary_error) => {
                let staged_salt = storage.read_staged_salt(Self::DB_NAME);
                let Some(staged_salt) = staged_salt else {
                    return Err(primary_error);
                };

                let wallet = Self::load_with_salt(storage.clone(), staged_salt, network, password)
                    .map_err(|_| primary_error)?;

                storage.commit_staged_salt(Self::DB_NAME)?;
                tracing::warn!(
                    "Completed a password change that was interrupted before its rotated salt was committed."
                );
                Ok(wallet)
            }
        }
    }

    fn build_tx(&mut self) -> TxBuilder<'_, AlwaysSpendImportedFirst> {
        self.build_tx()
    }

    fn get_new_address(&mut self) -> Result<AddressInfo> {
        self.next_address(KeychainKind::External)
    }

    fn get_change_address(&mut self) -> Result<AddressInfo> {
        self.next_address(KeychainKind::Internal)
    }

    fn balance(&self) -> Amount {
        self.available_balance().trusted_spendable()
    }

    fn get_seed_phrase(&self, password: &Password) -> Result<Mnemonic> {
        if !self.check_password(password)? {
            return Err(WalletErrorKind::InvalidPassword);
        }

        Self::load_mnemonic(&self.db)
    }

    fn drain_imported_balance(&mut self, fee_rate: FeeRate) -> Result<Psbt> {
        let drain_to_address = self.next_address(KeychainKind::Internal)?;
        let imported_utxos = self.imported_utxos();
        let imported_amount: Amount = imported_utxos
            .iter()
            .map(|utxo| utxo.utxo.txout().value)
            .sum();
        let cs = SpendImportedOnly(imported_utxos.clone());

        let mut tx_builder = self.build_tx().coin_selection(cs);

        tx_builder
            .fee_rate(fee_rate)
            .add_recipient(drain_to_address.script_pubkey(), imported_amount);

        let psbt = match tx_builder.finish() {
            Err(CreateTxError::CoinSelection(insufficient_funds)) => {
                let cs = SpendImportedOnly(imported_utxos);
                let Some(fees) = insufficient_funds
                    .needed
                    .checked_sub(insufficient_funds.available)
                else {
                    return Err(
                        CreateTxError::CoinSelection(insufficient_funds)
                            .into(),
                    );
                };
                let Some(amount_to_send) = imported_amount.checked_sub(fees) else {
                    return Err(
                        CreateTxError::CoinSelection(insufficient_funds)
                            .into(),
                    );
                };
                let mut new_builder = self.build_tx().coin_selection(cs);
                new_builder
                    .fee_rate(fee_rate)
                    .add_recipient(drain_to_address.script_pubkey(), amount_to_send);
                new_builder.finish()?
            }
            Err(e) => return Err(e.into()),
            Ok(psbt) => psbt,
        };
        Ok(psbt)
    }
}

impl Deref for BMPWallet {
    type Target = PersistedWallet<Connection>;
    fn deref(&self) -> &Self::Target {
        &self.wallet
    }
}

impl DerefMut for BMPWallet {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.wallet
    }
}
