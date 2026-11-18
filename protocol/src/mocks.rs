use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bdk_wallet::bitcoin::bip32::Xpriv;
use bdk_wallet::bitcoin::hashes::Hash as _;
use bdk_wallet::bitcoin::key::{Keypair, Secp256k1};
use bdk_wallet::bitcoin::secp256k1::Message;
use bdk_wallet::bitcoin::sighash::{Prevouts, SighashCache};
use bdk_wallet::bitcoin::taproot::Signature;
use bdk_wallet::bitcoin::transaction::Version;
use bdk_wallet::bitcoin::{
    Address, Amount, FeeRate, Network, OutPoint, PrivateKey, Psbt, ScriptBuf, Sequence,
    TapSighashType, Transaction, TxIn, TxOut, Txid, VarInt, Weight, Witness, XOnlyPublicKey,
    absolute, psbt,
};
use bdk_wallet::miniscript::descriptor::TapTree;
use wallet::error::WalletErrorKind;
use wallet::key_shares::{TradeKeyShares, derive_trade_key_shares};
use wallet::protocol_wallet_api::ProtocolWalletApi;
use wallet::secret::SecretScalar;
use zeroize::Zeroizing;

use crate::psbt::Redact as _;
use crate::transaction::{TransactionErrorKind, TxOutput};

fn mock_error(kind: TransactionErrorKind) -> WalletErrorKind {
    WalletErrorKind::Generic(Box::new(kind))
}

struct MockTradeWallet<Cs: Iterator<Item = TxOutput>, As: Iterator<Item = Address>> {
    funding_coins: Cs,
    new_addresses: As,
    signature_map: BTreeMap<OutPoint, Signature>,
    internal_key: Option<XOnlyPublicKey>,
    /// The secret key of `internal_key`, the script key of the trader. The wallet signs a spend
    /// through the script path with it, so these signatures are valid.
    script_key: Keypair,
    /// The seed for the key shares of trades. Each mock wallet has its own seed.
    seed: [u8; 32],
    locked_utxos: BTreeSet<OutPoint>,
    trade_records: BTreeMap<String, Vec<u8>>,
    /// While this is set, the wallet cannot store a trade record, like a wallet with a full or
    /// failing disk.
    store_fails: Arc<AtomicBool>,
}

impl<Cs: Iterator<Item = TxOutput>, As: Iterator<Item = Address>> ProtocolWalletApi
    for MockTradeWallet<Cs, As>
{
    fn network(&self) -> Network { Network::Regtest }

    fn new_address(&mut self) -> Result<Address, WalletErrorKind> {
        self.new_addresses
            .next()
            .ok_or_else(|| mock_error(TransactionErrorKind::MissingAddress))
    }

    fn new_internal_key(&mut self) -> Result<XOnlyPublicKey, WalletErrorKind> {
        self.internal_key
            .take()
            .ok_or_else(|| mock_error(TransactionErrorKind::MissingAddress))
    }

    fn create_psbt(
        &mut self,
        mut recipients: Vec<(ScriptBuf, Amount)>,
        fee_rate: FeeRate,
    ) -> Result<Psbt, WalletErrorKind> {
        let fee_cost_msat = |weight: Weight| {
            fee_rate.to_sat_per_kwu().checked_mul(weight.to_wu())
                .ok_or(mock_error(TransactionErrorKind::Overflow))
        };

        // Provisionally add a change recipient of zero value. We should never normally use
        // `new_address()` for change outputs, but this is just a mock.
        recipients.push((self.new_address()?.script_pubkey(), Amount::ZERO));

        let base_weight = Weight::from_wu_usize(38 + 4 * VarInt::from(recipients.len()).size());
        let mut output = Vec::with_capacity(recipients.len());
        let mut cost_msat = fee_cost_msat(base_weight)?;

        for (script_pubkey, value) in recipients {
            let tx_out = TxOut { value, script_pubkey };
            cost_msat = (|| cost_msat
                .checked_add(value.to_sat().checked_mul(1000)?)?
                .checked_add(fee_cost_msat(tx_out.weight()).ok()?))()
                .ok_or(mock_error(TransactionErrorKind::Overflow))?;
            output.push(tx_out);
        }

        let mut input = Vec::new();
        let mut inputs = Vec::new();
        let mut funds = Amount::ZERO;

        while funds < Amount::from_sat(cost_msat.div_ceil(1000)) {
            let new_coin = self.funding_coins.next()
                .ok_or(mock_error(TransactionErrorKind::MissingTxOutput))?;
            let new_coin_weight = new_coin.estimated_input_weight()
                .ok_or(mock_error(TransactionErrorKind::InvalidPsbt))?;
            funds = funds.checked_add(new_coin.prevout.value)
                .ok_or(mock_error(TransactionErrorKind::Overflow))?;
            cost_msat = cost_msat.checked_add(fee_cost_msat(new_coin_weight)?)
                .ok_or(mock_error(TransactionErrorKind::Overflow))?;
            input.push(TxIn {
                previous_output: new_coin.outpoint,
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                ..TxIn::default()
            });
            inputs.push(psbt::Input {
                witness_utxo: Some(new_coin.prevout),
                ..psbt::Input::default()
            });
        }

        let change_output = output.last_mut().expect("tx has a provisional change output");
        change_output.value = funds - Amount::from_sat(cost_msat.div_ceil(1000));
        if change_output.value < change_output.script_pubkey.minimal_non_dust() {
            output.pop();
        }

        let unsigned_tx = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input,
            output,
        };
        Ok(Psbt { inputs, ..Psbt::from_unsigned_tx(unsigned_tx).expect("tx is unsigned by construction") })
    }

    fn sign_selected_inputs(
        &mut self,
        psbt: &mut Psbt,
        is_selected: &dyn Fn(&OutPoint) -> bool,
    ) -> Result<(), WalletErrorKind> {
        let secp = Secp256k1::new();
        let (script_key, _) = self.script_key.x_only_public_key();
        let unsigned_tx = psbt.unsigned_tx.clone();
        let prevouts = psbt
            .inputs
            .iter()
            .map(|input| input.witness_utxo.clone())
            .collect::<Option<Vec<_>>>();

        for (
            index,
            (
                input,
                TxIn {
                    previous_output, ..
                },
            ),
        ) in psbt.inputs.iter_mut().zip(&unsigned_tx.input).enumerate()
        {
            if is_selected(previous_output) {
                // Script spend with the script key of the trader. The signature has no random
                // part, like the signatures of the wallet library.
                let leaf_hashes = input.tap_key_origins.get(&script_key)
                    .map(|(leaf_hashes, _)| leaf_hashes.clone()).unwrap_or_default();
                for leaf_hash in leaf_hashes {
                    let prevouts = prevouts.as_ref()
                        .ok_or_else(|| mock_error(TransactionErrorKind::InvalidPsbt))?;
                    let sighash = SighashCache::new(&unsigned_tx)
                        .taproot_script_spend_signature_hash(
                            index, &Prevouts::All(prevouts), leaf_hash, TapSighashType::Default)
                        .map_err(|e| mock_error(e.into()))?;
                    let message = Message::from_digest(sighash.to_byte_array());
                    let signature = Signature {
                        signature: secp.sign_schnorr_no_aux_rand(&message, &self.script_key),
                        sighash_type: TapSighashType::Default,
                    };
                    input.tap_script_sigs.insert((script_key, leaf_hash), signature);
                }
                if let Some(signature) = self.signature_map.get(previous_output) {
                    // Mock keyspend:
                    input.final_script_witness = Some(Witness::p2tr_key_spend(signature));
                    input.redact_sensitive_fields();
                } else if input.tap_key_origins.len() == input.tap_script_sigs.len() + 1 {
                    // Mock script spend (assumes only one path):
                    if let Some((control_block, (script, _))) = input.tap_scripts.first_key_value()
                    {
                        let mut wit = Witness::new();
                        // For the purpose of the mock, assume that (pubkey, leaf-hash) -> signature
                        // mappings occur in the opposite order that the signatures need to be added
                        // to the witness. This won't be true in general.
                        for sig in input.tap_script_sigs.values().rev() {
                            wit.push(sig.serialize());
                        }
                        wit.push(script.as_bytes());
                        wit.push(control_block.serialize());
                        input.final_script_witness = Some(wit);
                        input.redact_sensitive_fields();
                    }
                }
            }
        }
        Ok(())
    }

    fn import_private_key(
        &mut self,
        _pk: SecretScalar,
        _tap_tree: Option<TapTree<XOnlyPublicKey>>,
    ) -> Result<(), WalletErrorKind> {
        // The mock holds no coins of imported keys, so the key is only accepted.
        Ok(())
    }

    fn chain_tip_height(&self) -> u32 {
        0
    }

    fn record_transaction(&mut self, _tx: &Transaction) -> Result<(), WalletErrorKind> {
        Ok(())
    }

    fn forget_transaction(&mut self, _txid: Txid) -> Result<(), WalletErrorKind> {
        Ok(())
    }

    /// The mock child spends the last output of the parent, which is the output of the trader in
    /// the transactions of a trade. It has no signature and no output. Its lock time is the fee
    /// rate that was asked for, in sat per 1000 weight units, so that a test can read it.
    fn create_fee_bump_child(
        &mut self,
        parent: &Transaction,
        _parent_fee: Amount,
        package_fee_rate: FeeRate,
        _max_child_fee: Amount,
    ) -> Result<Transaction, WalletErrorKind> {
        let vout = u32::try_from(parent.output.len())
            .ok()
            .and_then(|len| len.checked_sub(1))
            .ok_or_else(|| mock_error(TransactionErrorKind::MissingTxOutput))?;
        let fee_rate = u32::try_from(package_fee_rate.to_sat_per_kwu())
            .map_err(|_| mock_error(TransactionErrorKind::Overflow))?;
        Ok(Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::from_height(fee_rate)
                .map_err(|_| mock_error(TransactionErrorKind::Overflow))?,
            input: vec![TxIn {
                previous_output: OutPoint::new(parent.compute_txid(), vout),
                ..TxIn::default()
            }],
            output: Vec::new(),
        })
    }

    fn trade_key_shares(&self, trade_id: &str) -> wallet::error::Result<TradeKeyShares> {
        let master_key = Xpriv::new_master(Network::Regtest, &self.seed)?;
        derive_trade_key_shares(&master_key, trade_id)
    }

    fn store_trade_record(&mut self, trade_id: &str, record: &[u8]) -> wallet::error::Result<()> {
        if self.store_fails.load(Ordering::Relaxed) {
            return Err(WalletErrorKind::Unsupported(
                "the mock wallet was told to fail",
            ));
        }
        self.trade_records
            .insert(trade_id.to_owned(), record.to_vec());
        Ok(())
    }

    fn trade_records(&mut self) -> wallet::error::Result<Vec<(String, Zeroizing<Vec<u8>>)>> {
        Ok(self
            .trade_records
            .iter()
            .map(|(trade_id, record)| (trade_id.clone(), Zeroizing::new(record.clone())))
            .collect())
    }

    fn lock_utxo(&mut self, utxo: OutPoint) -> wallet::error::Result<()> {
        self.locked_utxos.insert(utxo);
        Ok(())
    }

    fn list_locked_utxos(&self) -> Vec<OutPoint> {
        self.locked_utxos.iter().copied().collect()
    }

    fn unlock_utxo(&mut self, utxo: OutPoint) -> wallet::error::Result<()> {
        self.locked_utxos.remove(&utxo);
        Ok(())
    }

    fn is_utxo_locked(&self, utxo: OutPoint) -> bool {
        self.locked_utxos.contains(&utxo)
    }
}

pub fn mock_buyer_trade_wallet() -> impl ProtocolWalletApi {
    mock_buyer_trade_wallet_with_store_switch(Arc::default())
}

pub fn mock_seller_trade_wallet() -> impl ProtocolWalletApi {
    mock_seller_trade_wallet_with_store_switch(Arc::default())
}

/// As [`mock_buyer_trade_wallet`]. While `store_fails` is set, the wallet cannot store a trade
/// record.
//noinspection SpellCheckingInspection
pub fn mock_buyer_trade_wallet_with_store_switch(
    store_fails: Arc<AtomicBool>,
) -> impl ProtocolWalletApi {
    let funding_coins = [
        "658654575bbbeb46e965bd9eb37fd3be550a7e0fa2d64bc5f218763155602300:0",
    ].map(TxOutput::mock_1_btc_coin).into_iter();
    let signature_map = signature_map(funding_coins.as_slice(), &[
        "f631ae99b4743315b237af9c48ae1f9bb87b6c5404e84d8e3907269218d1bba5\
         4c397158aa233fd3f2227f4dc46922ef62eb8cc39a06b7a339b33e2401d512c1",
    ]);
    let new_addresses = [
        "bcrt1pgsj9aw0wvs5h6zj780djnu267v6jazmfekwm4g4q4s6ax3w3t0lseqqnjc",
        "bcrt1pkar3gerekw8f9gef9vn9xz0qypytgacp9wa5saelpksdgct33qdqan7c89",
        "bcrt1pv537m7m6w0gdrcdn3mqqdpgrk3j400yrdrjwf5c9whyl2f8f4p6q9dn3l9",
        "bcrt1pzvynlely05x82u40cts3znctmvyskue74xa5zwy0t5ueuv92726szpgpaa",
    ].map(|a| a.parse::<Address<_>>()
        .expect("hardcoded addresses should be valid").assume_checked()).into_iter();
    // The key of `tr(cPZzKuNmpuUjD1e8jUU4PVzy2b5LngbSip8mBsxf4e7rSFZVb4Uh)`.
    let script_key = script_key("cPZzKuNmpuUjD1e8jUU4PVzy2b5LngbSip8mBsxf4e7rSFZVb4Uh");
    let internal_key = Some(script_key.x_only_public_key().0);

    MockTradeWallet {
        funding_coins,
        new_addresses,
        signature_map,
        internal_key,
        script_key,
        seed: [1; 32],
        locked_utxos: BTreeSet::new(),
        trade_records: BTreeMap::new(),
        store_fails,
    }
}

/// As [`mock_seller_trade_wallet`]. While `store_fails` is set, the wallet cannot store a trade
/// record.
//noinspection SpellCheckingInspection
pub fn mock_seller_trade_wallet_with_store_switch(
    store_fails: Arc<AtomicBool>,
) -> impl ProtocolWalletApi {
    let funding_coins = [
        "4a5ecc72ec8db78f11c6785f560a13f6f32eac66d160a8157d30956695ccf523:0",
        "373b3ca0b9135e9649672772d4659bb5597d06b4694f1fbdbece285823fde0a3:1",
    ].map(TxOutput::mock_1_btc_coin).into_iter();
    let signature_map = signature_map(funding_coins.as_slice(), &[
        "2376111ed79dac9ff6f2d85dfe57d142f6075f4df9381aeab87a941477425224\
         7555f791ddf82354a3d73fa24955f6c5330ae44b2b238fa74be2eee9a46fcb72",
        "637f4a624bfc46bdb52e01b923d157a97148210f1a2669716815d3249c615673\
         17d543868698f30130e0aaf693ce265c544e3db5eda568bffb6ccd03d25a31df",
    ]);
    let new_addresses = [
        "bcrt1p80xu5f0nqjarfnechsmlt488jf3tykx8cva9zeeczlsu4c7x557qr499gz",
        "bcrt1pt5xd4aqe9whmvlz78mt39rvdlgpp6hujs5ggwan5285zjnsf73rq20k456",
        "bcrt1pwxlp4v9v7v03nx0e7vunlc87d4936wnyqegw0fuahudypan64wysefpqzy",
        "bcrt1pw4s5zvfm665fq9u6uwn9g7gwna658s939dvvf9wg63yede8kvyms5pmalx",
        "bcrt1pe3kcs085e8qej9aqqx6qryv2qsfxzywy9xd8pryzwemv2dghdqgscylr69",
    ].map(|a| a.parse::<Address<_>>()
        .expect("hardcoded addresses should be valid").assume_checked()).into_iter();
    // The key of `tr(cNaQCDwmmh4dS9LzCgVtyy1e1xjCJ21GUDHe9K98nzb689JvinGV)`.
    let script_key = script_key("cNaQCDwmmh4dS9LzCgVtyy1e1xjCJ21GUDHe9K98nzb689JvinGV");
    let internal_key = Some(script_key.x_only_public_key().0);

    MockTradeWallet {
        funding_coins,
        new_addresses,
        signature_map,
        internal_key,
        script_key,
        seed: [2; 32],
        locked_utxos: BTreeSet::new(),
        trade_records: BTreeMap::new(),
        store_fails,
    }
}

fn signature_vec(signatures: &[&'static str]) -> Vec<Signature> {
    signatures.iter().map(|s| Signature {
        signature: s.parse().expect("hardcoded signatures should be valid"),
        sighash_type: TapSighashType::Default,
    }).collect()
}

fn signature_map(funding_coins: &[TxOutput], signatures: &[&'static str]) -> BTreeMap<OutPoint, Signature> {
    funding_coins.iter().map(|o| o.outpoint).zip(signature_vec(signatures)).collect()
}

/// The key pair of a private key in wallet import format.
fn script_key(wif: &str) -> Keypair {
    let private_key: PrivateKey = wif.parse().expect("hardcoded keys should be valid");
    Keypair::from_secret_key(&Secp256k1::new(), &private_key.inner)
}
