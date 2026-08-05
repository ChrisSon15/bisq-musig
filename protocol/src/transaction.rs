use std::collections::BTreeSet;

use bdk_wallet::bitcoin::amount::CheckedSum as _;
use bdk_wallet::bitcoin::policy::MAX_STANDARD_TX_WEIGHT;
use bdk_wallet::bitcoin::psbt::ExtractTxError;
use bdk_wallet::bitcoin::sighash::{Prevouts, SighashCache};
use bdk_wallet::bitcoin::taproot::{Signature, TAPROOT_ANNEX_PREFIX};
use bdk_wallet::bitcoin::transaction::Version;
use bdk_wallet::bitcoin::{
    Address, Amount, FeeRate, Network, OutPoint, Psbt, Sequence, TapSighash, TapSighashType,
    Transaction, TxIn, TxOut, Txid, Weight, Witness, absolute, relative, script,
};
use bdk_wallet::miniscript::DefiniteDescriptorKey;
use bdk_wallet::miniscript::psbt::PsbtInputExt as _;
use paste::paste;
use rand::RngCore;
use relative::LockTime;
use thiserror::Error;
use wallet::protocol_wallet_api::ProtocolWalletApi;

use crate::psbt;
use crate::receiver::ReceiverList;

pub const REGTEST_WARNING_LOCK_TIME: LockTime = LockTime::from_height(5);
pub const REGTEST_REDIRECT_LOCK_TIME: LockTime = LockTime::ZERO;
pub const REGTEST_CLAIM_LOCK_TIME: LockTime = LockTime::from_height(5);

// TODO: Consider further (also, we may want different delays for fiat & altcoin trades):
pub const MAINNET_WARNING_LOCK_TIME: LockTime = LockTime::from_height(144 * 10);
// TODO: Consider further (zero disables relative lock time):
pub const MAINNET_REDIRECT_LOCK_TIME: LockTime = LockTime::ZERO;
// TODO: Consider further:
pub const MAINNET_CLAIM_LOCK_TIME: LockTime = LockTime::from_height(144 * 5);

pub const ANCHOR_AMOUNT: Amount = Amount::from_sat(330);
pub const SIGNED_FORWARDING_TX_WEIGHT: Weight = Weight::from_wu(444);
pub const SIGNED_WARNING_TX_WEIGHT: Weight = Weight::from_wu(846);
pub const SIGNED_REDIRECT_TX_BASE_WEIGHT: Weight = SIGNED_FORWARDING_TX_WEIGHT;
pub const SIGNED_CUSTOM_PAYOUT_TX_WEIGHT: Weight = Weight::from_wu(1182);

pub trait NetworkParams {
    fn warning_lock_time(&self) -> LockTime;

    fn redirect_lock_time(&self) -> LockTime;

    fn claim_lock_time(&self) -> LockTime;
}

impl NetworkParams for Network {
    fn warning_lock_time(&self) -> LockTime {
        match self {
            Self::Regtest => REGTEST_WARNING_LOCK_TIME,
            _ => MAINNET_WARNING_LOCK_TIME
        }
    }

    fn redirect_lock_time(&self) -> LockTime {
        match self {
            Self::Regtest => REGTEST_REDIRECT_LOCK_TIME,
            _ => MAINNET_REDIRECT_LOCK_TIME
        }
    }

    fn claim_lock_time(&self) -> LockTime {
        match self {
            Self::Regtest => REGTEST_CLAIM_LOCK_TIME,
            _ => MAINNET_CLAIM_LOCK_TIME
        }
    }
}

macro_rules! make_getter {
    ($field_name:ident: $field_type:ident) => {
        make_getter!($field_name: $field_type: $field_type);
    };
    ($field_name:ident: $field_type:ident: $base_field_type:ident) => {
        paste! {
            pub fn $field_name(&self) -> Result<&$field_type> {
                self.$field_name.as_ref().ok_or(TransactionErrorKind::[<Missing $base_field_type>])
            }
        }
    };
}

macro_rules! make_setter {
    ($field_name:ident: $field_type:ident) => {
        paste! {
            pub fn [<set_ $field_name>](&mut self, $field_name: $field_type) -> &mut Self {
                self.$field_name.get_or_insert($field_name);
                self
            }
        }
    };
}

macro_rules! make_getter_setter {
    ($field_name:ident: $field_type:ident) => {
        make_getter!($field_name: $field_type);
        make_setter!($field_name: $field_type);
    };
}

#[derive(Clone, Debug)]
pub struct TxOutput {
    pub outpoint: OutPoint,
    pub prevout: TxOut,
}

impl TxOutput {
    pub const fn new(outpoint: OutPoint, prevout: TxOut) -> Self { Self { outpoint, prevout } }

    pub(crate) fn estimated_input_weight(&self) -> Option<Weight> {
        Some(Weight::from_wu(match &self.prevout.script_pubkey {
            // Assumes a keyspend with default sighash type:
            // NOTE: We're trusting the peer to make only keyspends for P2TR Deposit inputs, since
            //  they should never be using the script paths of the outputs of previous trades (and
            //  their keychain addresses are keyspend-only anyway). To actually enforce a bound on
            //  the satisfaction weight would leak metadata and lead to consistent fee overpayment.
            //  (The worst they can do is cause the Deposit Tx to get stuck, which is impossible to
            //  prevent without additionally checking the entire ancestor tree of unconfirmed txs.)
            // weight = 230 = 4 * (36 + 1 + 4) (non-witness part) + 2 + 64 (signature)
            s if s.is_p2tr() => 230,
            // Assumes low-R nonce grinding:
            // weight = 271 = 4 * (36 + 1 + 4) (non-witness part) + 3 + 33 (pubkey) + 71 (signature)
            s if s.is_p2wpkh() => 271,
            // Other spk types are forbidden, as they lead to either tx malleability or an
            // arbitrarily long and unstructured witness program, or no standard spends:
            _ => return None
        }))
    }

    pub(crate) fn mock_1_btc_coin(outpoint: &'static str) -> Self {
        let script_pubkey = script::Builder::new().push_int(1).push_slice([0; 32]).into_script();
        Self {
            outpoint: outpoint.parse().expect("for mocking only; assumed valid outpoint str"),
            prevout: TxOut { value: Amount::ONE_BTC, script_pubkey },
        }
    }
}

pub trait TransactionExt: Sized {
    /// # Panics
    /// Will panic if `input_index` is out of bounds
    #[must_use]
    fn with_key_spend_witness(self, input_index: usize, signature: &Signature) -> Self;

    fn key_spend_signature(&self, input_index: usize) -> Result<Signature>;

    fn find_key_spend_signature(&self, input: &TxOutput) -> Result<Signature>;

    fn check_no_dust_outputs(&self) -> Result<()>;

    fn check_standard_weight(&self) -> Result<()>;
}

impl TransactionExt for Transaction {
    fn with_key_spend_witness(mut self, input_index: usize, signature: &Signature) -> Self {
        self.input[input_index].witness = Witness::p2tr_key_spend(signature);
        self
    }

    fn key_spend_signature(&self, input_index: usize) -> Result<Signature> {
        let witness = &self.tx_in(input_index)?.witness;
        match (witness.len(), witness.last()) {
            (1, Some(sl)) => {
                Ok(Signature::from_slice(sl)?)
            }
            (2, Some(sl)) if sl.starts_with(&[TAPROOT_ANNEX_PREFIX]) => {
                Ok(Signature::from_slice(witness.second_to_last().expect("len > 1"))?)
            }
            _ => Err(TransactionErrorKind::InvalidWitness)
        }
    }

    fn find_key_spend_signature(&self, input: &TxOutput) -> Result<Signature> {
        for (input_index, TxIn { previous_output, .. }) in self.input.iter().enumerate() {
            if previous_output == &input.outpoint {
                return self.key_spend_signature(input_index);
            }
        }
        Err(TransactionErrorKind::MissingSignature)
    }

    fn check_no_dust_outputs(&self) -> Result<()> {
        for (i, o) in self.output.iter().enumerate() {
            if o.value < o.script_pubkey.minimal_non_dust() {
                return Err(TransactionErrorKind::DustOutput(o.value, i));
            }
        }
        Ok(())
    }

    fn check_standard_weight(&self) -> Result<()> {
        let weight = self.weight();
        if weight.to_wu() > u64::from(MAX_STANDARD_TX_WEIGHT) {
            return Err(TransactionErrorKind::NonstandardTxWeight(weight));
        }
        Ok(())
    }
}

trait WithFixedInputs<const N: usize> {
    fn inputs(&self) -> Result<[&TxOutput; N]>;

    fn tx_ins(&self, lock_time: LockTime) -> Result<[TxIn; N]> {
        let sequence = if lock_time == LockTime::ZERO {
            Sequence::ENABLE_RBF_NO_LOCKTIME
        } else {
            lock_time.to_sequence()
        };
        Ok(self.inputs()?.map(|input|
            TxIn { previous_output: input.outpoint, sequence, ..TxIn::default() }))
    }

    fn key_spend_sighash(&self, tx: &Transaction, input_index: usize) -> Result<TapSighash> {
        let prevouts = self.inputs()?.map(|input| &input.prevout);
        let prevouts = Prevouts::All(&prevouts);
        let mut cache = SighashCache::new(tx);
        // TODO: Report missing validation to rust-bitcoin if index is not correct.
        Ok(cache.taproot_key_spend_signature_hash(input_index, &prevouts, TapSighashType::Default)?)
    }
}

#[derive(Default)]
pub struct DepositTxBuilder {
    // Supplied fields:
    trade_amount: Option<Amount>,
    buyers_security_deposit: Option<Amount>,
    sellers_security_deposit: Option<Amount>,
    buyer_payout_address: Option<Address>,
    seller_payout_address: Option<Address>,
    trade_fee_receivers: Option<ReceiverList>,
    fee_rate: Option<FeeRate>,
    // Externally derived fields:
    buyers_half_psbt: Option<Psbt>,
    sellers_half_psbt: Option<Psbt>,
    // Derived fields:
    psbt: Option<Psbt>,
    buyer_payout: Option<TxOutput>,
    seller_payout: Option<TxOutput>,
}

impl DepositTxBuilder {
    make_getter_setter!(trade_amount: Amount);
    make_getter_setter!(buyers_security_deposit: Amount);
    make_getter_setter!(sellers_security_deposit: Amount);
    make_getter_setter!(buyer_payout_address: Address);
    make_getter_setter!(seller_payout_address: Address);
    make_getter_setter!(trade_fee_receivers: ReceiverList);
    make_getter_setter!(fee_rate: FeeRate);
    make_getter_setter!(buyers_half_psbt: Psbt);
    make_getter_setter!(sellers_half_psbt: Psbt);
    make_getter!(psbt: Psbt: Transaction);
    make_getter!(buyer_payout: TxOutput: Transaction);
    make_getter!(seller_payout: TxOutput: Transaction);

    fn sellers_trade_deposit(&self) -> Result<Amount> {
        self.trade_amount()?.checked_add(*self.sellers_security_deposit()?)
            .ok_or(TransactionErrorKind::Overflow)
    }

    pub fn init_buyers_half_psbt(
        &mut self,
        wallet: &mut (impl ProtocolWalletApi + ?Sized),
        rng: &mut dyn RngCore,
    ) -> Result<&mut Self> {
        let deposit_amount = *self.buyers_security_deposit()?;
        let fee_rate = *self.fee_rate()?;
        Ok(self.set_buyers_half_psbt(
            psbt::create_half_deposit_psbt(wallet, deposit_amount, fee_rate, &[], rng)?))
    }

    pub fn init_sellers_half_psbt(
        &mut self,
        wallet: &mut (impl ProtocolWalletApi + ?Sized),
        rng: &mut dyn RngCore,
    ) -> Result<&mut Self> {
        let deposit_amount = self.sellers_trade_deposit()?;
        let fee_rate = *self.fee_rate()?;
        let trade_fee_receivers = self.trade_fee_receivers()?;
        Ok(self.set_sellers_half_psbt(
            psbt::create_half_deposit_psbt(wallet, deposit_amount, fee_rate, trade_fee_receivers, rng)?))
    }

    pub fn compute_unsigned_tx(&mut self) -> Result<&mut Self> {
        // Check that the placeholder output & receiver outputs of each PSBT half are correct.
        let [buyer_psbt, seller_psbt] = [self.buyers_half_psbt()?, self.sellers_half_psbt()?];
        psbt::check_placeholder_output(buyer_psbt, *self.buyers_security_deposit()?)?;
        psbt::check_placeholder_output(seller_psbt, self.sellers_trade_deposit()?)?;
        psbt::check_receiver_outputs(seller_psbt, self.trade_fee_receivers()?)?;

        // Compute a raw merged PSBT.
        let target_fee_rate = *self.fee_rate()?;
        let num_receivers = self.trade_fee_receivers()?.len();
        let mut psbt = psbt::merge_psbt_halves(buyer_psbt, seller_psbt, target_fee_rate, num_receivers)?;

        // Set the payout outputs of the PSBT and shuffle all its inputs and outputs.
        let mut buyer_payout = TxOutput::new(OutPoint::null(), TxOut {
            value: self.buyers_security_deposit()?.checked_add(*self.trade_amount()?)
                .ok_or(TransactionErrorKind::Overflow)?,
            script_pubkey: self.buyer_payout_address()?.script_pubkey(),
        });
        let mut seller_payout = TxOutput::new(OutPoint::null(), TxOut {
            value: *self.sellers_security_deposit()?,
            script_pubkey: self.seller_payout_address()?.script_pubkey(),
        });
        psbt::set_payouts_and_shuffle(&mut psbt, &mut buyer_payout, &mut seller_payout);
        psbt.unsigned_tx.check_no_dust_outputs()?;

        // Initialize the computed fields.
        self.psbt = Some(psbt);
        self.buyer_payout = Some(buyer_payout);
        self.seller_payout = Some(seller_payout);
        Ok(self)
    }

    pub fn txid(&self) -> Result<&Txid> { Ok(&self.buyer_payout()?.outpoint.txid) }

    pub fn sign_buyer_inputs(
        &mut self,
        wallet: &mut (impl ProtocolWalletApi + ?Sized),
    ) -> Result<&mut Self> {
        self.sign_matching_inputs(wallet, &psbt::prevout_set(self.buyers_half_psbt()?))
    }

    pub fn sign_seller_inputs(
        &mut self,
        wallet: &mut (impl ProtocolWalletApi + ?Sized),
    ) -> Result<&mut Self> {
        self.sign_matching_inputs(wallet, &psbt::prevout_set(self.sellers_half_psbt()?))
    }

    fn sign_matching_inputs(
        &mut self,
        wallet: &mut (impl ProtocolWalletApi + ?Sized),
        prevouts: &BTreeSet<OutPoint>,
    ) -> Result<&mut Self> {
        let psbt = self.psbt.as_mut().ok_or(TransactionErrorKind::MissingTransaction)?;
        wallet.sign_selected_inputs(psbt, &|o| prevouts.contains(o))?;
        Ok(self)
    }

    pub fn combine_psbts(&mut self, other: Psbt) -> Result<&mut Self> {
        self.psbt.as_mut().ok_or(TransactionErrorKind::MissingTransaction)?.combine(other)?;
        Ok(self)
    }

    pub fn signed_tx(&self) -> Result<Transaction> { psbt::extract_signed_tx(self.psbt()?) }
}

#[derive(Default)]
pub struct WarningTxBuilder {
    // Supplied fields:
    buyer_input: Option<TxOutput>,
    seller_input: Option<TxOutput>,
    escrow_address: Option<Address>,
    anchor_address: Option<Address>,
    lock_time: Option<LockTime>,
    fee_rate: Option<FeeRate>,
    buyer_input_signature: Option<Signature>,
    seller_input_signature: Option<Signature>,
    // Derived fields:
    unsigned_tx: Option<Transaction>,
    signed_tx: Option<Transaction>,
    txid: Option<Txid>,
}

impl WarningTxBuilder {
    make_getter_setter!(buyer_input: TxOutput);
    make_getter_setter!(seller_input: TxOutput);
    make_getter_setter!(escrow_address: Address);
    make_getter_setter!(anchor_address: Address);
    make_getter_setter!(lock_time: LockTime);
    make_getter_setter!(fee_rate: FeeRate);
    make_getter_setter!(buyer_input_signature: Signature);
    make_getter_setter!(seller_input_signature: Signature);
    make_getter!(unsigned_tx: Transaction);
    make_getter!(signed_tx: Transaction);
    make_getter!(txid: Txid: Transaction);

    pub fn escrow_amount(input_amounts: impl IntoIterator<Item = Amount>, fee_rate: FeeRate) -> Result<Amount> {
        (|| input_amounts.into_iter().checked_sum()?
            .checked_sub(ANCHOR_AMOUNT)?
            .checked_sub(fee_rate.checked_mul_by_weight(SIGNED_WARNING_TX_WEIGHT)?)
        )().ok_or(TransactionErrorKind::Overflow)
    }

    pub fn compute_unsigned_tx(&mut self) -> Result<&mut Self> {
        let escrow_output = TxOut {
            value: Self::escrow_amount(
                self.inputs()?.map(|input| input.prevout.value),
                *self.fee_rate()?)?,
            script_pubkey: self.escrow_address()?.script_pubkey(),
        };
        let anchor_output = TxOut {
            value: ANCHOR_AMOUNT,
            script_pubkey: self.anchor_address()?.script_pubkey(),
        };
        let tx = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: self.tx_ins(*self.lock_time()?)?.to_vec(),
            output: vec![escrow_output, anchor_output],
        };
        tx.check_no_dust_outputs()?;
        self.txid = Some(self.unsigned_tx.get_or_insert(tx).compute_txid());
        Ok(self)
    }

    pub fn escrow(&self) -> Result<TxOutput> {
        let output = self.unsigned_tx()?.output[0].clone();
        Ok(TxOutput::new(OutPoint::new(*self.txid()?, 0), output))
    }

    pub fn buyer_input_sighash(&self) -> Result<TapSighash> {
        self.key_spend_sighash(self.unsigned_tx()?, 0)
    }

    pub fn seller_input_sighash(&self) -> Result<TapSighash> {
        self.key_spend_sighash(self.unsigned_tx()?, 1)
    }

    pub fn compute_signed_tx(&mut self) -> Result<&mut Self> {
        let tx = self.unsigned_tx()?.clone()
            .with_key_spend_witness(0, self.buyer_input_signature()?)
            .with_key_spend_witness(1, self.seller_input_signature()?);
        self.signed_tx.get_or_insert(tx);
        Ok(self)
    }
}

impl WithFixedInputs<2> for WarningTxBuilder {
    fn inputs(&self) -> Result<[&TxOutput; 2]> { Ok([self.buyer_input()?, self.seller_input()?]) }
}

#[derive(Default)]
pub struct RedirectTxBuilder {
    // Supplied fields:
    input: Option<TxOutput>,
    receivers: Option<ReceiverList>,
    anchor_address: Option<Address>,
    lock_time: Option<LockTime>,
    input_signature: Option<Signature>,
    // Derived fields:
    unsigned_tx: Option<Transaction>,
    signed_tx: Option<Transaction>,
    txid: Option<Txid>,
}

impl RedirectTxBuilder {
    make_getter_setter!(input: TxOutput);
    make_getter_setter!(receivers: ReceiverList);
    make_getter_setter!(anchor_address: Address);
    make_getter_setter!(lock_time: LockTime);
    make_getter_setter!(input_signature: Signature);
    make_getter!(unsigned_tx: Transaction);
    make_getter!(signed_tx: Transaction);
    make_getter!(txid: Txid: Transaction);

    pub fn available_amount_msat(escrow_amount: Amount, fee_rate: FeeRate) -> Result<u64> {
        (|| {
            let redirection_tx_base_fee = fee_rate.to_sat_per_kwu()
                .checked_mul(SIGNED_REDIRECT_TX_BASE_WEIGHT.to_wu())?;
            escrow_amount
                .checked_sub(ANCHOR_AMOUNT)?
                .to_sat().checked_mul(1000)?
                .checked_sub(redirection_tx_base_fee)
        })().ok_or(TransactionErrorKind::Overflow)
    }

    pub fn compute_unsigned_tx(&mut self) -> Result<&mut Self> {
        let mut output = Vec::with_capacity(self.receivers()?.len() + 1);
        output.extend(self.receivers()?.iter().map(TxOut::from));
        output.push(TxOut {
            value: ANCHOR_AMOUNT,
            script_pubkey: self.anchor_address()?.script_pubkey(),
        });
        let tx = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: self.tx_ins(*self.lock_time()?)?.to_vec(),
            output,
        };
        tx.check_no_dust_outputs()?;
        self.txid = Some(self.unsigned_tx.get_or_insert(tx).compute_txid());
        Ok(self)
    }

    pub fn input_sighash(&self) -> Result<TapSighash> {
        self.key_spend_sighash(self.unsigned_tx()?, 0)
    }

    pub fn compute_signed_tx(&mut self) -> Result<&mut Self> {
        let tx = self.unsigned_tx()?.clone().with_key_spend_witness(0, self.input_signature()?);
        tx.check_standard_weight()?;
        self.signed_tx.get_or_insert(tx);
        Ok(self)
    }
}

impl WithFixedInputs<1> for RedirectTxBuilder {
    fn inputs(&self) -> Result<[&TxOutput; 1]> { Ok([self.input()?]) }
}

#[derive(Default)]
pub struct ForwardingTxBuilder {
    // Supplied fields:
    input: Option<TxOutput>,
    payout_address: Option<Address>,
    lock_time: Option<LockTime>,
    fee_rate: Option<FeeRate>,
    input_signature: Option<Signature>,
    // Derived fields:
    unsigned_tx: Option<Transaction>,
    signed_tx: Option<Transaction>,
}

pub type SwapTxBuilder = ForwardingTxBuilder;
pub type ClaimTxBuilder = ForwardingTxBuilder;
pub type PenaltyTxBuilder = ForwardingTxBuilder;

impl ForwardingTxBuilder {
    make_getter_setter!(input: TxOutput);
    make_getter_setter!(payout_address: Address);
    make_getter_setter!(lock_time: LockTime);
    make_getter_setter!(fee_rate: FeeRate);
    make_getter_setter!(input_signature: Signature);
    make_getter!(unsigned_tx: Transaction);
    make_getter!(signed_tx: Transaction);

    pub fn disable_lock_time(&mut self) -> &mut Self { self.set_lock_time(LockTime::ZERO) }

    fn payout_amount(input_amount: Amount, fee_rate: FeeRate) -> Option<Amount> {
        input_amount.checked_sub(fee_rate.checked_mul_by_weight(SIGNED_FORWARDING_TX_WEIGHT)?)
    }

    pub fn compute_unsigned_tx(&mut self) -> Result<&mut Self> {
        let output = vec![TxOut {
            value: Self::payout_amount(self.input()?.prevout.value, *self.fee_rate()?)
                .ok_or(TransactionErrorKind::Overflow)?,
            script_pubkey: self.payout_address()?.script_pubkey(),
        }];
        let tx = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: self.tx_ins(*self.lock_time()?)?.to_vec(),
            output,
        };
        tx.check_no_dust_outputs()?;
        self.unsigned_tx.get_or_insert(tx);
        Ok(self)
    }

    pub fn input_sighash(&self) -> Result<TapSighash> {
        self.key_spend_sighash(self.unsigned_tx()?, 0)
    }

    pub fn compute_signed_tx(&mut self) -> Result<&mut Self> {
        let tx = self.unsigned_tx()?.clone().with_key_spend_witness(0, self.input_signature()?);
        self.signed_tx.get_or_insert(tx);
        Ok(self)
    }
}

impl WithFixedInputs<1> for ForwardingTxBuilder {
    fn inputs(&self) -> Result<[&TxOutput; 1]> { Ok([self.input()?]) }
}

type Descriptor = bdk_wallet::miniscript::Descriptor<DefiniteDescriptorKey>;

#[derive(Default)]
pub struct CustomPayoutTxBuilder {
    // Supplied fields:
    buyer_input: Option<TxOutput>,
    seller_input: Option<TxOutput>,
    buyer_input_descriptor: Option<Descriptor>,
    seller_input_descriptor: Option<Descriptor>,
    buyer_payout_address: Option<Address>,
    seller_payout_address: Option<Address>,
    seller_payout_amount_excluding_fee: Option<Amount>,
    fee_rate: Option<FeeRate>,
    // Derived fields:
    psbt: Option<Psbt>,
}

impl CustomPayoutTxBuilder {
    make_getter_setter!(buyer_input: TxOutput);
    make_getter_setter!(seller_input: TxOutput);
    make_getter_setter!(buyer_input_descriptor: Descriptor);
    make_getter_setter!(seller_input_descriptor: Descriptor);
    make_getter_setter!(buyer_payout_address: Address);
    make_getter_setter!(seller_payout_address: Address);
    make_getter_setter!(seller_payout_amount_excluding_fee: Amount);
    make_getter_setter!(fee_rate: FeeRate);
    make_getter!(psbt: Psbt: Transaction);

    fn payout_amounts(
        input_amounts: impl IntoIterator<Item = Amount>,
        seller_payout_amount_excluding_fee: Amount,
        signed_tx_weight: Weight,
        fee_rate: FeeRate,
    ) -> Option<[Amount; 2]> {
        let total_fee = fee_rate.checked_mul_by_weight(signed_tx_weight)?;
        let seller_payout = seller_payout_amount_excluding_fee
            .checked_sub(total_fee / 2)?;
        let buyer_payout = input_amounts.into_iter().checked_sum()?.checked_sub(seller_payout)?
            .checked_sub(total_fee)?;
        Some([buyer_payout, seller_payout])
    }

    pub fn compute_unsigned_tx(&mut self) -> Result<&mut Self> {
        const P2TR_SPK_WEIGHT: Weight = Weight::from_wu(34 * 4);
        let buyer_payout_spk = self.buyer_payout_address()?.script_pubkey();
        let seller_payout_spk = self.seller_payout_address()?.script_pubkey();

        // In exceptional cases, the payout addresses may not in fact be P2TR, so correct for that
        // in the final tx weight estimate and resultant tx fee & payout amounts.
        let signed_tx_weight = SIGNED_CUSTOM_PAYOUT_TX_WEIGHT
            + Weight::from_wu_usize(buyer_payout_spk.len() * 4) - P2TR_SPK_WEIGHT
            + Weight::from_wu_usize(seller_payout_spk.len() * 4) - P2TR_SPK_WEIGHT;

        let [buyer_payout_amount, seller_payout_amount] = Self::payout_amounts(
            self.inputs()?.map(|input| input.prevout.value),
            *self.seller_payout_amount_excluding_fee()?,
            signed_tx_weight,
            *self.fee_rate()?,
        ).ok_or(TransactionErrorKind::Overflow)?;
        let buyer_payout = TxOut {
            value: buyer_payout_amount,
            script_pubkey: buyer_payout_spk,
        };
        let seller_payout = TxOut {
            value: seller_payout_amount,
            script_pubkey: seller_payout_spk,
        };
        let tx = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: self.tx_ins(LockTime::ZERO)?.to_vec(),
            output: vec![buyer_payout, seller_payout],
        };
        tx.check_no_dust_outputs()?;
        let mut psbt = Psbt::from_unsigned_tx(tx).expect("tx is unsigned by construction");
        psbt.inputs[0].witness_utxo = Some(self.buyer_input()?.prevout.clone());
        psbt.inputs[1].witness_utxo = Some(self.seller_input()?.prevout.clone());
        psbt.inputs[0].update_with_descriptor_unchecked(self.buyer_input_descriptor()?)?;
        psbt.inputs[1].update_with_descriptor_unchecked(self.seller_input_descriptor()?)?;
        self.psbt = Some(psbt);
        Ok(self)
    }

    pub fn sign_partial(
        &mut self,
        wallet: &mut (impl ProtocolWalletApi + ?Sized),
    ) -> Result<&mut Self> {
        let psbt = self.psbt.as_mut().ok_or(TransactionErrorKind::MissingTransaction)?;
        wallet.sign_selected_inputs(psbt, &|_| true)?;
        Ok(self)
    }

    pub fn combine_psbts(&mut self, other: Psbt) -> Result<&mut Self> {
        self.psbt.as_mut().ok_or(TransactionErrorKind::MissingTransaction)?.combine(other)?;
        Ok(self)
    }

    pub fn signed_tx(&self) -> Result<Transaction> { psbt::extract_signed_tx(self.psbt()?) }
}

impl WithFixedInputs<2> for CustomPayoutTxBuilder {
    fn inputs(&self) -> Result<[&TxOutput; 2]> { Ok([self.buyer_input()?, self.seller_input()?]) }
}

pub(crate) type Result<T, E = TransactionErrorKind> = std::result::Result<T, E>;

#[derive(Error, Debug)]
#[error(transparent)]
#[non_exhaustive]
pub enum TransactionErrorKind {
    #[error("missing lock time")]
    MissingLockTime,
    #[error("missing tx output")]
    MissingTxOutput,
    #[error("missing descriptor")]
    MissingDescriptor,
    #[error("missing address")]
    MissingAddress,
    #[error("missing BTC amount")]
    MissingAmount,
    #[error("missing receiver list")]
    MissingReceiverList,
    #[error("missing fee rate")]
    MissingFeeRate,
    #[error("missing PSBT")]
    MissingPsbt,
    #[error("missing transaction")]
    MissingTransaction,
    #[error("missing signature")]
    MissingSignature,
    #[error("overflow")]
    Overflow,
    #[error("invalid witness")]
    InvalidWitness,
    #[error("invalid PSBT")]
    InvalidPsbt,
    #[error("dust output of {0} at index {1}")]
    DustOutput(Amount, usize),
    #[error("transaction weight {0} exceeds policy maximum")]
    NonstandardTxWeight(Weight),
    AddressParse(#[from] bdk_wallet::bitcoin::address::ParseError),
    Taproot(#[from] bdk_wallet::bitcoin::sighash::TaprootError),
    InputsIndex(#[from] bdk_wallet::bitcoin::transaction::InputsIndexError),
    SigFromSlice(#[from] bdk_wallet::bitcoin::taproot::SigFromSliceError),
    Psbt(#[from] bdk_wallet::bitcoin::psbt::Error),
    ExtractTx(#[from] Box<ExtractTxError>),
    Miniscript(#[from] bdk_wallet::miniscript::Error),
    Conversion(#[from] bdk_wallet::miniscript::descriptor::ConversionError),
    Wallet(#[from] wallet::protocol_wallet_api::WalletErrorKind),
}

impl From<ExtractTxError> for TransactionErrorKind {
    fn from(error: ExtractTxError) -> Self { Box::new(error).into() }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bdk_wallet::bitcoin::consensus;
    use bdk_wallet::bitcoin::hex::test_hex_unwrap as hex;
    use const_format::str_index;
    use rand::SeedableRng as _;
    use rand_chacha::ChaCha20Rng;

    use super::*;
    use crate::mocks::{mock_buyer_trade_wallet, mock_seller_trade_wallet};
    use crate::receiver::Receiver;
    use crate::script_paths::deposit_payout_descriptor;

    // Valid signed txs pulled from an integration test run. We should be able to rebuild them exactly...

    //noinspection SpellCheckingInspection
    const SIGNED_DEPOSIT_TX: &str = "\
        0200000000010300236055317618f2c54bd6a20f7e0a55bed37fb39ebd65e946ebbb5b575486650000000000fdffffff\
        23f5cc956695307d15a860d166ac2ef3f6130a565f78c6118fb78dec72cc5e4a0000000000fdffffffa3e0fd235828ce\
        bebd1f4f69b4067d59b59b65d472276749965e13b9a03c3b370100000000fdffffff0470769303000000002251203bcd\
        ca25f304ba34cf38bc37f5d4e79262b258c7c33a51673817e1cae3c6a53cf3a7c4040000000022512044245eb9ee6429\
        7d0a5e3bdb29f15af3352e8b69cd9dbaa2a0ac35d345d15bff003b58080000000022512050ae873bc1665acda868f35e\
        205cbac0d37e71ad2c1ff8930f2cf1de9b7c131b002d3101000000002251206667c68404a48b32eb80a4d117d96c8310\
        6d02ca0d8fccd6ba0348aaacf4aa260140f631ae99b4743315b237af9c48ae1f9bb87b6c5404e84d8e3907269218d1bb\
        a54c397158aa233fd3f2227f4dc46922ef62eb8cc39a06b7a339b33e2401d512c101402376111ed79dac9ff6f2d85dfe\
        57d142f6075f4df9381aeab87a9414774252247555f791ddf82354a3d73fa24955f6c5330ae44b2b238fa74be2eee9a4\
        6fcb720140637f4a624bfc46bdb52e01b923d157a97148210f1a2669716815d3249c61567317d543868698f30130e0aa\
        f693ce265c544e3db5eda568bffb6ccd03d25a31df00000000";

    //noinspection SpellCheckingInspection
    const SIGNED_SWAP_TX: &str = "\
        0200000000010123a9879fc1262d360aaee74c72610cc5d5cf326fbc8d3656b51f34ad800909440300000000fdffffff\
        0118293101000000002251204d19b6cd267c57f719f2f3f01c2ce230505e053a0d20884f4aa9115ebff3338d01400555\
        c9a53b9f233186937e090d6116dd0e5215742ed56d44bd7dce0c10b40cf4c61a573b1fd1f68ca386f7ea9e420f8bd3c2\
        552a661222e1792d63caaad3353200000000";

    //noinspection SpellCheckingInspection
    const SIGNED_SELLERS_WARNING_TX: &str = "\
        0200000000010223a9879fc1262d360aaee74c72610cc5d5cf326fbc8d3656b51f34ad80090944020000000002000000\
        23a9879fc1262d360aaee74c72610cc5d5cf326fbc8d3656b51f34ad8009094403000000000200000002ce6289090000\
        00002251206667c68404a48b32eb80a4d117d96c83106d02ca0d8fccd6ba0348aaacf4aa264a01000000000000225120\
        4d19b6cd267c57f719f2f3f01c2ce230505e053a0d20884f4aa9115ebff3338d0140623b8be648c7fb330062ae29c30e\
        9cc13c7a18b756095490c6f673f900c56a8ba0759711fe395a5b98e053dd1e3bc39dbe2d5c2c1f162d9ab5f24144f61d\
        31810140bcc2a8b0aab906ae6f32444c4e7ac163153d134f1075a4153fe735fd83ff9e172b16b14efd5cb5995f1e3553\
        c9ceed572a3419e4aab7a2dd86cdb440181c597000000000";

    //noinspection SpellCheckingInspection
    const SIGNED_BUYERS_REDIRECT_TX: &str = "\
        02000000000101f703021407bbdc217f56b8256dd620e0252cc83eb9eacba7370b96091ccc20c60000000000fdffffff\
        0388d2b8050000000022512039ee586be03c9d1cddbffc83e874f4b38212fd841fdfef2dcc94fb8b5abf32df5c8cd003\
        00000000225120bdfe7695e32dfa072d8e561951925f3aff300d953cee127884b428de6b63d68c4a0100000000000022\
        51206661819664b49934b8a84a665b6d61c1d36a4537bb0fe074843864abd21aa0dd01407881f2345129f9305e4efe84\
        5d043ba98d0538352ffc6a50db35ca661b50ba6fd3e5553bdc71e27ba274ce0b862db53dde316da92614b91e8208fe1e\
        164ae0b400000000";

    //noinspection SpellCheckingInspection
    const SIGNED_SELLERS_CLAIM_TX: &str = "\
        02000000000101f703021407bbdc217f56b8256dd620e0252cc83eb9eacba7370b96091ccc20c6000000000002000000\
        01e65e8909000000002251204d19b6cd267c57f719f2f3f01c2ce230505e053a0d20884f4aa9115ebff3338d0140b1ec\
        0464f04ced9ce0d5715f3740b24fd4226f72c2d990b2bbdad29898e15051082ffc3f3626170a992e7a5a8b48fffd58e4\
        e5520b2c45d5b09a6f07c4c2a2dc00000000";

    const SWAP_TX_SIGNATURE: &str = str_index!(SIGNED_SWAP_TX, 188..316);
    const SELLERS_WARNING_TX_BUYER_INPUT_SIGNATURE: &str = str_index!(SIGNED_SELLERS_WARNING_TX, 356..484);
    const SELLERS_WARNING_TX_SELLER_INPUT_SIGNATURE: &str = str_index!(SIGNED_SELLERS_WARNING_TX, 488..616);
    const BUYERS_REDIRECT_TX_SIGNATURE: &str = str_index!(SIGNED_BUYERS_REDIRECT_TX, 360..488);
    const SELLERS_CLAIM_TX_SIGNATURE: &str = str_index!(SIGNED_SELLERS_CLAIM_TX, 188..316);

    //noinspection SpellCheckingInspection
    #[test]
    fn test_txids() {
        let deposit_tx = tx(SIGNED_DEPOSIT_TX);
        let swap_tx = tx(SIGNED_SWAP_TX);
        let warning_tx = tx(SIGNED_SELLERS_WARNING_TX);
        let redirect_tx = tx(SIGNED_BUYERS_REDIRECT_TX);
        let claim_tx = tx(SIGNED_SELLERS_CLAIM_TX);

        let [deposit_txid, swap_txid, warning_txid, redirect_txid, claim_txid] =
            [&deposit_tx, &swap_tx, &warning_tx, &redirect_tx, &claim_tx].map(Transaction::compute_txid);

        assert_eq!("44090980ad341fb556368dbc6f32cfd5c50c61724ce7ae0a362d26c19f87a923", deposit_txid.to_string());
        assert_eq!("41e2045e54060bfe2e094e76a7ce4fbd7185384d34afcb77353cd359b465c93d", swap_txid.to_string());
        assert_eq!("c620cc1c09960b37a7cbeab93ec82c25e020d66d25b8567f21dcbb07140203f7", warning_txid.to_string());
        assert_eq!("ecdb3995689d8e23e4c467692ab400af66eb9f5a35b99753ee3bfdaa10b0b1d6", redirect_txid.to_string());
        assert_eq!("81be3cf683c4a65979bbbe35264c8d0e4cd6fe33e80d094463f13ecf4d5eb2df", claim_txid.to_string());
    }

    #[test]
    fn test_deposit_tx_builder() -> Result<()> {
        let builder = filled_deposit_tx_builder(false)?;
        let signed_tx = builder.signed_tx()?;

        assert_eq!(tx(SIGNED_DEPOSIT_TX), signed_tx);
        Ok(())
    }

    #[test]
    fn test_swap_tx_builder() -> Result<()> {
        let builder = filled_swap_tx_builder(&filled_deposit_tx_builder(false)?)?;
        let signed_tx = builder.signed_tx()?;

        assert_eq!(&tx(SIGNED_SWAP_TX), signed_tx);
        Ok(())
    }

    #[test]
    fn test_warning_tx_builder() -> Result<()> {
        let builder = filled_warning_tx_builder(&filled_deposit_tx_builder(false)?)?;
        let signed_tx = builder.signed_tx()?;

        assert_eq!(&tx(SIGNED_SELLERS_WARNING_TX), signed_tx);
        Ok(())
    }

    #[test]
    fn test_redirect_tx_builder() -> Result<()> {
        let builder = filled_redirect_tx_builder(
            &filled_warning_tx_builder(&filled_deposit_tx_builder(false)?)?)?;
        let signed_tx = builder.signed_tx()?;

        assert_eq!(&tx(SIGNED_BUYERS_REDIRECT_TX), signed_tx);
        Ok(())
    }

    #[test]
    fn test_claim_tx_builder() -> Result<()> {
        let builder = filled_claim_tx_builder(
            &filled_warning_tx_builder(&filled_deposit_tx_builder(false)?)?)?;
        let signed_tx = builder.signed_tx()?;

        assert_eq!(&tx(SIGNED_SELLERS_CLAIM_TX), signed_tx);
        Ok(())
    }

    //noinspection SpellCheckingInspection
    #[test]
    fn test_custom_payout_tx_builder() -> Result<()> {
        let mut fake_buyer_trade_wallet = bdk_wallet::test_utils::get_funded_wallet_single(
            "tr(cPZzKuNmpuUjD1e8jUU4PVzy2b5LngbSip8mBsxf4e7rSFZVb4Uh)").0;
        let mut fake_seller_trade_wallet = bdk_wallet::test_utils::get_funded_wallet_single(
            "tr(cNaQCDwmmh4dS9LzCgVtyy1e1xjCJ21GUDHe9K98nzb689JvinGV)").0;

        let mut builder0 = filled_unsigned_custom_payout_tx_builder(
            &filled_deposit_tx_builder(true)?)?;
        let mut builder1 = filled_unsigned_custom_payout_tx_builder(
            &filled_deposit_tx_builder(true)?)?;

        // Check that signing with the mock buyer/seller wallets has exactly the same effect as
        // signing with the above fake BDK trade wallets (with the particular params chosen)...

        builder0.sign_partial(&mut fake_buyer_trade_wallet)?;
        builder1.sign_partial(&mut mock_buyer_trade_wallet())?;
        assert_eq!(builder0.psbt()?, builder1.psbt()?);

        builder0.sign_partial(&mut fake_seller_trade_wallet)?;
        builder1.sign_partial(&mut mock_seller_trade_wallet())?;
        assert_eq!(builder0.psbt()?, builder1.psbt()?);

        // Now check the final signed tx...

        let signed_tx = builder0.signed_tx()?;
        assert_eq!(SIGNED_CUSTOM_PAYOUT_TX_WEIGHT, signed_tx.weight());
        Ok(())
    }

    fn tx(hex: &str) -> Transaction { consensus::deserialize(&hex!(hex)).unwrap() }

    fn sig(hex: &str) -> Signature { Signature::from_slice(&hex!(hex)).unwrap() }

    //noinspection SpellCheckingInspection
    fn filled_deposit_tx_builder(for_custom_payout: bool) -> Result<DepositTxBuilder> {
        let [buyer_payout_address, seller_payout_address] = if for_custom_payout {
            ["bcrt1ppavdcmx98s6xg86csr9t5nkul746kvhpplkveylsfeg7g88csm3qfyegr3"; 2]
        } else {
            [
                "bcrt1p2zhgww7pvedvm2rg7d0zqh96crfhuudd9s0l3yc09ncaaxmuzvds5zhh8t",
                "bcrt1pvenudpqy5j9n96uq5ng30ktvsvgx6qk2pk8ue446qdy24t854gnq5sp2l6",
            ]
        }.map(|addr| addr.parse::<Address<_>>().unwrap().assume_checked());

        // The RNG seed determines the shuffling of the deposit inputs & outputs. The byte 0x96 is
        // the smallest (of the two) array fill values which happen to give the correct shuffling.
        let mut rng = ChaCha20Rng::from_seed([0x96; 32]);

        let mut builder = DepositTxBuilder::default();
        builder
            .set_trade_amount(Amount::from_sat(119_999_984))
            .set_buyers_security_deposit(Amount::from_sat(20_000_016))
            .set_sellers_security_deposit(Amount::from_sat(20_000_000))
            .set_buyer_payout_address(buyer_payout_address)
            .set_seller_payout_address(seller_payout_address)
            .set_trade_fee_receivers(ReceiverList::default())
            .set_fee_rate(FeeRate::from_sat_per_kwu(5158)) // gives 7325-sat absolute fee
            .init_buyers_half_psbt(&mut mock_buyer_trade_wallet(), &mut rng)?
            .init_sellers_half_psbt(&mut mock_seller_trade_wallet(), &mut rng)?
            .compute_unsigned_tx()?
            .sign_buyer_inputs(&mut mock_buyer_trade_wallet())?
            .sign_seller_inputs(&mut mock_seller_trade_wallet())?;
        Ok(builder)
    }

    //noinspection SpellCheckingInspection
    fn filled_swap_tx_builder(deposit_tx_builder: &DepositTxBuilder) -> Result<SwapTxBuilder> {
        let payout_address = "bcrt1pf5vmdnfx03tlwx0j70cpct8zxpg9upf6p5sgsn624yg4a0lnxwxsegnwnx"
            .parse::<Address<_>>()?.require_network(Network::Regtest)?;

        let mut builder = SwapTxBuilder::default();
        builder
            .set_input(deposit_tx_builder.seller_payout()?.clone())
            .set_payout_address(payout_address)
            .disable_lock_time()
            .set_fee_rate(FeeRate::from_sat_per_kwu(2252)) // gives 1000-sat absolute fee
            .compute_unsigned_tx()?
            .set_input_signature(sig(SWAP_TX_SIGNATURE))
            .compute_signed_tx()?;
        Ok(builder)
    }

    //noinspection SpellCheckingInspection
    fn filled_warning_tx_builder(deposit_tx_builder: &DepositTxBuilder) -> Result<WarningTxBuilder> {
        let escrow_address = "bcrt1pvenudpqy5j9n96uq5ng30ktvsvgx6qk2pk8ue446qdy24t854gnq5sp2l6"
            .parse::<Address<_>>()?.require_network(Network::Regtest)?;
        let anchor_address = "bcrt1pf5vmdnfx03tlwx0j70cpct8zxpg9upf6p5sgsn624yg4a0lnxwxsegnwnx"
            .parse::<Address<_>>()?.require_network(Network::Regtest)?;

        let mut builder = WarningTxBuilder::default();
        builder
            .set_buyer_input(deposit_tx_builder.buyer_payout()?.clone())
            .set_seller_input(deposit_tx_builder.seller_payout()?.clone())
            .set_escrow_address(escrow_address)
            .set_anchor_address(anchor_address)
            .set_lock_time(LockTime::from_height(2))
            .set_fee_rate(FeeRate::from_sat_per_kwu(1182)) // gives 1000-sat absolute fee
            .compute_unsigned_tx()?
            .set_buyer_input_signature(sig(SELLERS_WARNING_TX_BUYER_INPUT_SIGNATURE))
            .set_seller_input_signature(sig(SELLERS_WARNING_TX_SELLER_INPUT_SIGNATURE))
            .compute_signed_tx()?;
        Ok(builder)
    }

    //noinspection SpellCheckingInspection
    fn filled_redirect_tx_builder(warning_tx_builder: &WarningTxBuilder) -> Result<RedirectTxBuilder> {
        let receiver_address1 = "bcrt1p88h9s6lq8jw3ehdlljp7sa85kwpp9lvyrl077twvjnackk4lxt0sffnlrk"
            .parse::<Address<_>>()?.require_network(Network::Regtest)?;
        let receiver_address2 = "bcrt1phhl8d90r9haqwtvw2cv4ryjl8tlnqrv48nhpy7yyks5du6mr66xq5nlwhz"
            .parse::<Address<_>>()?.require_network(Network::Regtest)?;
        let anchor_address = "bcrt1pvescr9nykjvnfw9gffn9kmtpc8fk53fhhv87qayy8pj2h5s65rwsjm9ja4"
            .parse::<Address<_>>()?.require_network(Network::Regtest)?;
        let receivers = [
            Receiver { address: receiver_address1, amount: Amount::from_sat(95_998_600) },
            Receiver { address: receiver_address2, amount: Amount::from_sat(63_999_068) },
        ];

        let mut builder = RedirectTxBuilder::default();
        builder
            .set_input(warning_tx_builder.escrow()?)
            .set_receivers(Arc::new(receivers))
            .set_anchor_address(anchor_address)
            .set_lock_time(LockTime::ZERO)
            .compute_unsigned_tx()?
            .set_input_signature(sig(BUYERS_REDIRECT_TX_SIGNATURE))
            .compute_signed_tx()?;
        Ok(builder)
    }

    //noinspection SpellCheckingInspection
    fn filled_claim_tx_builder(warning_tx_builder: &WarningTxBuilder) -> Result<ClaimTxBuilder> {
        let payout_address = "bcrt1pf5vmdnfx03tlwx0j70cpct8zxpg9upf6p5sgsn624yg4a0lnxwxsegnwnx"
            .parse::<Address<_>>()?.require_network(Network::Regtest)?;

        let mut builder = ClaimTxBuilder::default();
        builder
            .set_input(warning_tx_builder.escrow()?)
            .set_payout_address(payout_address)
            .set_lock_time(LockTime::from_height(2))
            .set_fee_rate(FeeRate::from_sat_per_kwu(2252)) // gives 1000-sat absolute fee
            .compute_unsigned_tx()?
            .set_input_signature(sig(SELLERS_CLAIM_TX_SIGNATURE))
            .compute_signed_tx()?;
        Ok(builder)
    }

    //noinspection SpellCheckingInspection
    fn filled_unsigned_custom_payout_tx_builder(deposit_tx_builder: &DepositTxBuilder) -> Result<CustomPayoutTxBuilder> {
        let buyer_input_internal_key =
            "b511bd5771e47ee27558b1765e87b541668304ec567721c7b880edc0a010da55".parse().unwrap();
        let seller_input_internal_key =
            "b511bd5771e47ee27558b1765e87b541668304ec567721c7b880edc0a010da55".parse().unwrap();
        let buyer_pub_key =
            "51494dc22e24a32fe9dcfbd7e85faf345fa1df296fb49d156e859ef345201295".parse().unwrap();
        let seller_pub_key =
            "fcba7ecf41bc7e1be4ee122d9d22e3333671eb0a3a87b5cdf099d59874e1940f".parse().unwrap();

        let buyer_input_descriptor = deposit_payout_descriptor(
            &buyer_input_internal_key, &buyer_pub_key, &seller_pub_key)?;
        let seller_input_descriptor = deposit_payout_descriptor(
            &seller_input_internal_key, &buyer_pub_key, &seller_pub_key)?;

        let buyer_payout_address = "bcrt1p2zhgww7pvedvm2rg7d0zqh96crfhuudd9s0l3yc09ncaaxmuzvds5zhh8t"
            .parse::<Address<_>>()?.require_network(Network::Regtest)?;
        let seller_payout_address = "bcrt1pvenudpqy5j9n96uq5ng30ktvsvgx6qk2pk8ue446qdy24t854gnq5sp2l6"
            .parse::<Address<_>>()?.require_network(Network::Regtest)?;

        let mut builder = CustomPayoutTxBuilder::default();
        builder
            .set_buyer_input(deposit_tx_builder.buyer_payout()?.clone())
            .set_seller_input(deposit_tx_builder.seller_payout()?.clone())
            .set_buyer_input_descriptor(buyer_input_descriptor)
            .set_seller_input_descriptor(seller_input_descriptor)
            .set_buyer_payout_address(buyer_payout_address)
            .set_seller_payout_address(seller_payout_address)
            .set_seller_payout_amount_excluding_fee(Amount::from_sat(30_000_000))
            .set_fee_rate(FeeRate::from_sat_per_vb_u32(15))
            .compute_unsigned_tx()?;
        Ok(builder)
    }
}
