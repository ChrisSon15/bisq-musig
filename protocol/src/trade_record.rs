//! The stored form of a trade: what the trade store keeps, and how a trade is built again from
//! it after a restart. The rules are specified in
//! `docs/specifications/trade/persistence-and-recovery.md`.
//!
//! A record holds the data that the trader received and the results of the steps that cannot be
//! repeated: his addresses, his half of the deposit, his public nonces, his partial signatures
//! and the signed PSBTs. Everything else is computed again when the trade is loaded, with the
//! same steps as during the setup. A record that does not fit these computations is refused.
//!
//! Never part of a record:
//! - the trader's own secret key shares. They are derived from the seed of the wallet when the
//!   trade is loaded (D10);
//! - secret nonces. After the trader signed the deposit transaction, no signature of the trade
//!   needs a nonce any more.
//!
//! A record can contain the secret key share of the peer. The wallet stores records encrypted.

use bdk_wallet::bitcoin::hex::{DisplayHex as _, FromHex as _};
use bdk_wallet::serde_json;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize as _, Zeroizing};

use super::*;

/// The version of the format. A record of another version is refused.
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    trade_id: String,
    role: String,
    /// The state that the requests gave the trade. The state that follows from the chain is not
    /// stored: it is derived from the chain again.
    state: String,
    terminated: Option<String>,
    reserved_coins: Vec<String>,
    deposit_signing_height: Option<u32>,
    deposit_watch_start: Option<u32>,
    my_script_key: String,
    /// The data of the setup. A trade whose deposit transaction the trader has not signed is
    /// stored without it: its setup cannot continue after a restart, because its nonces are lost.
    setup: Option<Setup>,
}

#[derive(Serialize, Deserialize)]
struct Setup {
    params: StoredParams,
    peer: PeerData,
    own: OwnData,
}

/// The trade parameters, the configuration of the engine at the time of the setup, and the other
/// data that the caller supplied.
#[derive(Serialize, Deserialize)]
struct StoredParams {
    trade_amount: u64,
    buyers_security_deposit: u64,
    sellers_security_deposit: u64,
    deposit_tx_fee_rate: u64,
    prepared_tx_fee_rate: u64,
    trade_fee_receiver: Option<(String, u64)>,
    warning_lock_time: u16,
    claim_lock_time: u16,
    deposit_timeout: u16,
    release_safety_margin: u16,
    forced_close_margin: u16,
    redirection_receivers: Vec<(String, u64)>,
    custom_payout: Option<CustomPayout>,
    penalty_tx_fee_rate: Option<u64>,
}

/// The data that the peer sent.
#[derive(Serialize, Deserialize)]
struct PeerData {
    buyer_payout_key_share: String,
    seller_payout_key_share: String,
    script_key: String,
    half_deposit_psbt: String,
    addresses: [String; 3],
    nonce_shares: Vec<String>,
    warning_tx_buyer_input_signature: String,
    warning_tx_seller_input_signature: String,
    redirect_tx_signature: String,
    /// For the buyer: the seller's partial signature and signature hash of the swap transaction.
    swap_tx_signature: Option<(String, String)>,
    /// For the seller: the buyer's partial signature for the swap transaction (message E).
    buyers_swap_tx_signature: Option<String>,
    /// The secret key share of the peer, once received (message F or G, or read from the swap
    /// transaction).
    secret_key_share: Option<String>,
}

impl Drop for PeerData {
    /// The text form of the secret key share is wiped with the record.
    fn drop(&mut self) {
        self.secret_key_share.zeroize();
    }
}

/// Serialises a record into a buffer that has its final size from the start, and that is wiped
/// when it is dropped. A buffer that grows would leave copies of its content, which can hold
/// the secret key share of the peer, in memory that it gave back.
fn serialize(record: &Record) -> Result<Zeroizing<Vec<u8>>> {
    struct Counter(usize);

    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, record).map_err(invalid)?;
    let mut serialized = Zeroizing::new(Vec::with_capacity(counter.0));
    serde_json::to_writer(&mut *serialized, record).map_err(invalid)?;
    Ok(serialized)
}

/// The results of the trader's own steps that cannot be repeated.
#[derive(Serialize, Deserialize)]
struct OwnData {
    warning_anchor_address: String,
    redirect_anchor_address: String,
    payout_address: String,
    swap_payout_address: Option<String>,
    half_deposit_psbt: String,
    nonce_shares: Vec<String>,
    partial_signatures: Vec<String>,
    deposit_psbt: String,
    payment_started: bool,
}

#[derive(Serialize, Deserialize)]
struct CustomPayout {
    sellers_payout_amount_excluding_fee: u64,
    fee_rate: u64,
    psbt: String,
}

const ROLES: [(Role, &str); 4] = [
    (Role::SellerAsMaker, "SellerAsMaker"),
    (Role::SellerAsTaker, "SellerAsTaker"),
    (Role::BuyerAsMaker, "BuyerAsMaker"),
    (Role::BuyerAsTaker, "BuyerAsTaker"),
];

fn invalid(what: impl Display) -> ProtocolErrorKind {
    ProtocolErrorKind::TradeRecord(what.to_string())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().to_lower_hex_string()
}

fn bytes(hex: &str, what: &str) -> Result<Vec<u8>> {
    Vec::from_hex(hex).map_err(|error| invalid(format!("{what}: {error}")))
}

/// Decodes a value from its bytes in hexadecimal form.
fn decode<'a, T: TryFrom<&'a [u8]>>(bytes: &'a [u8], what: &str) -> Result<T> {
    T::try_from(bytes).map_err(|_| invalid(format!("{what} cannot be decoded")))
}

fn script_key(hex: &str, what: &str) -> Result<XOnlyPublicKey> {
    XOnlyPublicKey::from_slice(&bytes(hex, what)?)
        .map_err(|error| invalid(format!("{what}: {error}")))
}

fn address(address: &str, network: Network, what: &str) -> Result<Address> {
    address
        .parse::<Address<NetworkUnchecked>>()
        .and_then(|address| address.require_network(network))
        .map_err(|error| invalid(format!("{what}: {error}")))
}

fn unchecked_address(address: &str, what: &str) -> Result<Address<NetworkUnchecked>> {
    address
        .parse()
        .map_err(|error| invalid(format!("{what}: {error}")))
}

fn psbt(hex: &str, what: &str) -> Result<Psbt> {
    Psbt::deserialize(&bytes(hex, what)?).map_err(|error| invalid(format!("{what}: {error}")))
}

fn partial_signature(hex: &str, what: &str) -> Result<PartialSignature> {
    decode(&bytes(hex, what)?, what)
}

fn receiver((address, amount): &(String, u64), what: &str) -> Result<Receiver<NetworkUnchecked>> {
    Ok(Receiver {
        address: unchecked_address(address, what)?,
        amount: Amount::from_sat(*amount),
    })
}

fn stored_receiver<V: NetworkValidation>(receiver: &Receiver<V>) -> (String, u64) {
    (
        receiver
            .address
            .as_unchecked()
            .assume_checked_ref()
            .to_string(),
        receiver.amount.to_sat(),
    )
}

impl TradeModel {
    /// The record of the trade for the trade store. It contains no secret key share of the
    /// trader and no secret nonce. It can contain the secret key share of the peer, so it is
    /// wiped when it is dropped.
    pub fn to_record(&self) -> Result<Zeroizing<Vec<u8>>> {
        let role = ROLES
            .iter()
            .find(|(role, _)| *role == self.my_role)
            .map(|(_, name)| (*name).to_owned())
            .expect("all roles are listed");
        let has_setup =
            self.state >= TradeState::Deposit && self.received.partial_signatures.is_some();
        let record = Record {
            version: VERSION,
            trade_id: self.trade_id.clone(),
            role,
            state: self.state.to_string(),
            terminated: self.terminated.clone(),
            reserved_coins: self
                .reserved_coins
                .iter()
                .map(ToString::to_string)
                .collect(),
            deposit_signing_height: self.deposit_signing_height,
            deposit_watch_start: self.deposit_watch_start,
            my_script_key: hex(self
                .keys
                .my_multisig_script_key
                .ok_or(ProtocolErrorKind::MissingScriptKey)?
                .serialize()),
            setup: has_setup.then(|| self.setup_record()).transpose()?,
        };
        serialize(&record)
    }

    fn setup_record(&self) -> Result<Setup> {
        Ok(Setup {
            params: self.stored_params()?,
            peer: self.peer_data()?,
            own: self.own_data()?,
        })
    }

    fn missing_setup() -> ProtocolErrorKind {
        invalid("the trade holds no data of its setup")
    }

    fn stored_params(&self) -> Result<StoredParams> {
        let params = self
            .received
            .params
            .as_ref()
            .ok_or_else(Self::missing_setup)?;
        let config = self.received.config.ok_or_else(Self::missing_setup)?;
        let nonce_shares = self
            .received
            .nonce_shares
            .as_ref()
            .ok_or_else(Self::missing_setup)?;
        let custom_payout = self
            .received
            .custom_payout_request
            .map(|(amount, fee_rate)| {
                Ok::<_, ProtocolErrorKind>(CustomPayout {
                    sellers_payout_amount_excluding_fee: amount.to_sat(),
                    fee_rate: fee_rate.to_sat_per_kwu(),
                    psbt: hex(self.custom_payout_tx.builder.psbt()?.serialize()),
                })
            })
            .transpose()?;
        Ok(StoredParams {
            trade_amount: params.trade_amount.to_sat(),
            buyers_security_deposit: params.buyers_security_deposit.to_sat(),
            sellers_security_deposit: params.sellers_security_deposit.to_sat(),
            deposit_tx_fee_rate: params.deposit_tx_fee_rate.to_sat_per_kwu(),
            prepared_tx_fee_rate: params.prepared_tx_fee_rate.to_sat_per_kwu(),
            trade_fee_receiver: params.trade_fee_receiver.as_ref().map(stored_receiver),
            warning_lock_time: config.warning_lock_time_blocks(),
            claim_lock_time: config.claim_lock_time_blocks(),
            deposit_timeout: config.deposit_timeout(),
            release_safety_margin: config.release_safety_margin(),
            forced_close_margin: config.forced_close_margin(),
            redirection_receivers: nonce_shares
                .redirection_receivers
                .iter()
                .map(stored_receiver)
                .collect(),
            custom_payout,
            penalty_tx_fee_rate: self
                .penalty_tx
                .builder
                .signed_tx()
                .ok()
                .and(self.penalty_tx.builder.fee_rate().ok())
                .map(|fee_rate| fee_rate.to_sat_per_kwu()),
        })
    }

    fn peer_data(&self) -> Result<PeerData> {
        let nonce_shares = self
            .received
            .nonce_shares
            .as_ref()
            .ok_or_else(Self::missing_setup)?;
        let signatures = self
            .received
            .partial_signatures
            .as_ref()
            .ok_or_else(Self::missing_setup)?;
        Ok(PeerData {
            buyer_payout_key_share: hex(self
                .keys
                .buyer_payout_ctx
                .peers_key_share()?
                .pub_key()
                .serialize()),
            seller_payout_key_share: hex(self
                .keys
                .seller_payout_ctx
                .peers_key_share()?
                .pub_key()
                .serialize()),
            script_key: hex(self
                .keys
                .peers_multisig_script_key
                .ok_or(ProtocolErrorKind::MissingScriptKey)?
                .serialize()),
            half_deposit_psbt: hex(nonce_shares.half_deposit_psbt.serialize()),
            addresses: nonce_shares
                .addresses
                .clone()
                .map(|address| address.assume_checked().to_string()),
            nonce_shares: nonce_shares
                .nonce_shares
                .iter()
                .map(|nonce| hex(nonce.serialize()))
                .collect(),
            warning_tx_buyer_input_signature: hex(signatures.warning_tx_buyer.serialize()),
            warning_tx_seller_input_signature: hex(signatures.warning_tx_seller.serialize()),
            redirect_tx_signature: hex(signatures.redirect_tx.serialize()),
            swap_tx_signature: signatures
                .swap_tx
                .map(|(signature, sighash)| (hex(signature.serialize()), hex(sighash))),
            buyers_swap_tx_signature: self
                .received
                .swap_tx_signature
                .map(|signature| hex(signature.serialize())),
            secret_key_share: self
                .keys
                .my_payout_ctx()
                .peers_key_share()?
                .prv_key()
                .ok()
                .map(|key_share| hex(key_share.to_bytes())),
        })
    }

    fn own_data(&self) -> Result<OwnData> {
        let my_txs = self.my_txs();
        let mut nonce_shares = Vec::new();
        let mut partial_signatures = Vec::new();
        for sig_ctx in self.all_sig_ctxs() {
            nonce_shares.push(hex(sig_ctx.my_nonce_share()?.serialize()));
            partial_signatures.push(hex(sig_ctx.my_partial_sig()?.serialize()));
        }
        Ok(OwnData {
            warning_anchor_address: my_txs.warning.builder.anchor_address()?.to_string(),
            redirect_anchor_address: my_txs.redirect.builder.anchor_address()?.to_string(),
            payout_address: my_txs.payout_address()?.to_string(),
            swap_payout_address: self
                .swap_tx
                .builder
                .payout_address()
                .ok()
                .map(ToString::to_string),
            half_deposit_psbt: hex(self.my_half_deposit_psbt()?.serialize()),
            nonce_shares,
            partial_signatures,
            deposit_psbt: hex(self.deposit_tx.builder.psbt()?.serialize()),
            payment_started: self.received.payment_started,
        })
    }

    /// Builds a trade again from its record. The wallet supplies the network and the trader's
    /// own key shares, which are derived from its seed.
    ///
    /// The transactions and signatures of the trade are computed again from the record, with
    /// the steps of the setup. A record whose data does not fit together is refused: for example
    /// a stored partial signature that is not valid for the transaction that the engine computes.
    pub fn from_record(record: &[u8], wallet: &dyn ProtocolWalletApi) -> Result<Self> {
        let record: Record = serde_json::from_slice(record).map_err(invalid)?;
        if record.version != VERSION {
            return Err(invalid(format!(
                "version {} is not the version {VERSION} of this program",
                record.version
            )));
        }
        let role = ROLES
            .iter()
            .find(|(_, name)| *name == record.role)
            .map(|(role, _)| *role)
            .ok_or_else(|| invalid(format!("unknown role {}", record.role)))?;
        let state = TradeState::VALUES
            .into_iter()
            .find(|state| state.to_string() == record.state)
            .ok_or_else(|| invalid(format!("unknown state {}", record.state)))?;
        let my_script_key = script_key(&record.my_script_key, "script key")?;
        let mut trade = Self::with_script_key(record.trade_id, role, wallet, my_script_key)?;
        trade.terminated = record.terminated;
        trade.reserved_coins = record
            .reserved_coins
            .iter()
            .map(|coin| coin.parse().map_err(invalid))
            .collect::<Result<_>>()?;
        trade.deposit_signing_height = record.deposit_signing_height;
        trade.deposit_watch_start = record.deposit_watch_start;
        match record.setup {
            Some(setup) => trade.restore_setup(&setup, state)?,
            None if state >= TradeState::Deposit => {
                return Err(invalid(format!(
                    "the record of a trade in state {state} holds no data of its setup"
                )));
            }
            None => {}
        }
        trade.state = state;
        Ok(trade)
    }

    /// Runs the steps of the setup again with the stored data, and then the later steps that the
    /// stored state implies.
    fn restore_setup(&mut self, setup: &Setup, state: TradeState) -> Result<()> {
        self.restore_keys_and_own_data(&setup.params, &setup.peer, &setup.own)?;
        let peers_swap_tx_signature = self.restore_exchanged_data(setup)?;
        self.restore_signatures(setup, peers_swap_tx_signature)?;
        self.restore_later_steps(setup, state)
    }

    /// Step 2 of the setup: key shares of the peer and trade parameters, and the trader's
    /// addresses, half of the deposit and public nonces.
    fn restore_keys_and_own_data(
        &mut self,
        stored: &StoredParams,
        peer: &PeerData,
        own: &OwnData,
    ) -> Result<()> {
        let network = self.network;
        let params = TradeParams {
            trade_amount: Amount::from_sat(stored.trade_amount),
            buyers_security_deposit: Amount::from_sat(stored.buyers_security_deposit),
            sellers_security_deposit: Amount::from_sat(stored.sellers_security_deposit),
            deposit_tx_fee_rate: FeeRate::from_sat_per_kwu(stored.deposit_tx_fee_rate),
            prepared_tx_fee_rate: FeeRate::from_sat_per_kwu(stored.prepared_tx_fee_rate),
            trade_fee_receiver: stored
                .trade_fee_receiver
                .as_ref()
                .map(|stored| receiver(stored, "trade fee receiver"))
                .transpose()?,
        };
        let config = EngineConfig::new(
            stored.warning_lock_time,
            stored.claim_lock_time,
            stored.deposit_timeout,
            stored.release_safety_margin,
        )
        .and_then(|config| config.with_forced_close_margin(stored.forced_close_margin))
        .map_err(invalid)?;
        let key_share =
            |hex: &str| decode::<Point>(&bytes(hex, "key share")?, "key share of the peer");
        let peers_key_shares = ExchangedKeys::<ByVal> {
            buyer_payout: key_share(&peer.buyer_payout_key_share)?,
            seller_payout: key_share(&peer.seller_payout_key_share)?,
            multisig_script: script_key(&peer.script_key, "script key of the peer")?,
        };
        self.set_peer_key_shares(&peers_key_shares);
        self.set_params(&params, config)?;
        self.aggregate_key_shares(config)?;

        if let Some(swap_payout_address) = &own.swap_payout_address {
            self.swap_tx.builder.set_payout_address(address(
                swap_payout_address,
                network,
                "swap payout address",
            )?);
        }
        let payout_address = address(&own.payout_address, network, "payout address")?;
        let warning_anchor = address(&own.warning_anchor_address, network, "anchor address")?;
        let redirect_anchor = address(&own.redirect_anchor_address, network, "anchor address")?;
        let my_txs = self.my_txs_mut();
        my_txs.warning.builder.set_anchor_address(warning_anchor);
        my_txs.redirect.builder.set_anchor_address(redirect_anchor);
        my_txs.payout_address = Some(payout_address.clone());
        self.penalty_tx.builder.set_payout_address(payout_address);
        let my_half_deposit_psbt = psbt(&own.half_deposit_psbt, "half deposit PSBT")?;
        if self.am_buyer() {
            self.deposit_tx
                .builder
                .set_buyers_half_psbt(my_half_deposit_psbt);
        } else {
            self.deposit_tx
                .builder
                .set_sellers_half_psbt(my_half_deposit_psbt);
        }
        let sig_ctx_count = self.all_sig_ctxs().len();
        if own.nonce_shares.len() != sig_ctx_count
            || peer.nonce_shares.len() != sig_ctx_count
            || own.partial_signatures.len() != sig_ctx_count
        {
            return Err(invalid("wrong number of nonces or partial signatures"));
        }
        for (sig_ctx, nonce) in self.all_sig_ctxs_mut().into_iter().zip(&own.nonce_shares) {
            sig_ctx.restore_my_nonce_share(decode(&bytes(nonce, "nonce")?, "own nonce")?);
        }
        self.received.params = Some(params);
        self.received.config = Some(config);
        Ok(())
    }

    /// Step 3 of the setup: half deposit, addresses and nonces of the peer, the redirection
    /// receivers, and the trader's partial signatures. Returns the seller's data for the swap
    /// transaction, if the trader is the buyer.
    fn restore_exchanged_data(
        &mut self,
        setup: &Setup,
    ) -> Result<Option<(PartialSignature, TapSighash)>> {
        let (peer, own) = (&setup.peer, &setup.own);
        let redirection_receivers = setup
            .params
            .redirection_receivers
            .iter()
            .map(|stored| receiver(stored, "redirection receiver"))
            .collect::<Result<Vec<_>>>()?;
        let mut peers_nonce_shares = Vec::with_capacity(peer.nonce_shares.len());
        for nonce in &peer.nonce_shares {
            peers_nonce_shares.push(decode(&bytes(nonce, "nonce")?, "nonce of the peer")?);
        }
        let peers_addresses =
            [0, 1, 2].map(|i| unchecked_address(&peer.addresses[i], "address of the peer"));
        let [warning_tx_fee_bump, redirect_tx_fee_bump, claim_tx_payout] = peers_addresses;
        let received = ReceivedNonceShares {
            half_deposit_psbt: psbt(&peer.half_deposit_psbt, "half deposit PSBT")?,
            addresses: [
                warning_tx_fee_bump?,
                redirect_tx_fee_bump?,
                claim_tx_payout?,
            ],
            nonce_shares: peers_nonce_shares
                .try_into()
                .map_err(|_| invalid("wrong number of nonces"))?,
            redirection_receivers: redirection_receivers.clone(),
        };
        self.set_redirection_receivers(&redirection_receivers)?;
        let [warning_tx_fee_bump, redirect_tx_fee_bump, claim_tx_payout] =
            received.addresses.clone();
        let peers_addresses = ExchangedAddresses {
            warning_tx_fee_bump,
            redirect_tx_fee_bump,
            claim_tx_payout,
        }
        .require_network(self.network)?;
        self.set_peer_half_deposit_psbt(received.half_deposit_psbt.clone());
        self.compute_unsigned_deposit_tx()?;
        self.set_peer_addresses(peers_addresses);
        self.compute_unsigned_prepared_txs()?;
        self.set_peer_nonce_shares(&received.nonce_shares);
        self.aggregate_nonce_shares()?;
        self.received.nonce_shares = Some(received);

        // The trader's partial signatures. A signature that does not belong to the computed
        // transaction is found afterwards: `sign_partial` would need the secret nonce for
        // another message, and the aggregation checks every signature.
        let my_partial_signatures = own
            .partial_signatures
            .iter()
            .map(|signature| partial_signature(signature, "own partial signature"))
            .collect::<Result<Vec<_>>>()?;
        let peers_swap_tx_signature = peer
            .swap_tx_signature
            .as_ref()
            .map(|(signature, sighash)| {
                let sighash: [u8; 32] = bytes(sighash, "signature hash")?
                    .try_into()
                    .map_err(|_| invalid("signature hash of the swap transaction"))?;
                Ok::<_, ProtocolErrorKind>((
                    partial_signature(signature, "partial signature of the peer")?,
                    TapSighash::from_byte_array(sighash),
                ))
            })
            .transpose()?;
        let swap_sighash = match peers_swap_tx_signature {
            Some((_, sighash)) => sighash,
            None => self.swap_tx.builder.input_sighash()?,
        };
        let sighashes = [
            swap_sighash,
            self.buyer_txs.warning.builder.buyer_input_sighash()?,
            self.buyer_txs.warning.builder.seller_input_sighash()?,
            self.seller_txs.warning.builder.buyer_input_sighash()?,
            self.seller_txs.warning.builder.seller_input_sighash()?,
            self.buyer_txs.redirect.builder.input_sighash()?,
            self.seller_txs.redirect.builder.input_sighash()?,
        ];
        for ((sig_ctx, sighash), signature) in self
            .all_sig_ctxs_mut()
            .into_iter()
            .zip(sighashes)
            .zip(my_partial_signatures)
        {
            sig_ctx.restore_my_partial_sig(sighash, signature);
        }
        self.sign_partial()?;
        Ok(peers_swap_tx_signature)
    }

    /// Step 4 of the setup: partial signatures of the peer, the trader's signed transactions,
    /// and the deposit transaction with its signatures.
    fn restore_signatures(
        &mut self,
        setup: &Setup,
        peers_swap_tx_signature: Option<(PartialSignature, TapSighash)>,
    ) -> Result<()> {
        let peer = &setup.peer;
        let peers_signature = |hex: &str| partial_signature(hex, "partial signature of the peer");
        let received = ReceivedPartialSignatures {
            warning_tx_buyer: peers_signature(&peer.warning_tx_buyer_input_signature)?,
            warning_tx_seller: peers_signature(&peer.warning_tx_seller_input_signature)?,
            redirect_tx: peers_signature(&peer.redirect_tx_signature)?,
            swap_tx: peers_swap_tx_signature,
        };
        if let Some((_, sighash)) = received.swap_tx {
            self.sign_swap_tx_input_partial(sighash)?;
        }
        self.set_peer_partial_signatures_on_my_txs(&received);
        self.aggregate_partial_signatures()?;
        self.compute_my_signed_prepared_txs()?;
        self.deposit_tx
            .builder
            .restore_psbt(psbt(&setup.own.deposit_psbt, "deposit PSBT")?)?;
        self.received.partial_signatures = Some(received);
        Ok(())
    }

    /// The steps after the setup that the stored data and the stored state imply.
    fn restore_later_steps(&mut self, setup: &Setup, state: TradeState) -> Result<()> {
        self.received.payment_started = setup.own.payment_started;
        if let Some(signature) = &setup.peer.buyers_swap_tx_signature {
            let signature = partial_signature(signature, "partial signature of the buyer")?;
            self.swap_tx.input_sig_ctx.set_peers_partial_sig(signature);
            self.swap_tx.input_sig_ctx.aggregate_partial_signatures()?;
            self.received.swap_tx_signature = Some(signature);
        }
        let seller_released = [
            TradeState::SellerReadyToRelease,
            TradeState::TradeClosed(ClosureType::Cooperative),
            TradeState::TradeClosed(ClosureType::Forced),
        ];
        if !self.am_buyer() && seller_released.contains(&state) {
            self.compute_signed_swap_tx()?;
        }
        if let Some(custom_payout) = &setup.params.custom_payout {
            let amount = Amount::from_sat(custom_payout.sellers_payout_amount_excluding_fee);
            let fee_rate = FeeRate::from_sat_per_kwu(custom_payout.fee_rate);
            self.custom_payout_tx
                .builder
                .set_seller_payout_amount_excluding_fee(amount)
                .set_fee_rate(fee_rate);
            self.compute_custom_payout_tx()?;
            self.custom_payout_tx
                .builder
                .restore_psbt(psbt(&custom_payout.psbt, "custom payout PSBT")?)?;
            self.received.custom_payout_request = Some((amount, fee_rate));
        }
        if let Some(key_share) = &setup.peer.secret_key_share {
            let key_share = Zeroizing::new(bytes(key_share, "secret key share")?);
            let key_share = SecretScalar::from_slice(&key_share)
                .map_err(|error| invalid(format!("secret key share of the peer: {error}")))?;
            self.keys.my_payout_ctx_mut().set_peers_prv_key(key_share)?;
            let fee_rate = setup.params.penalty_tx_fee_rate;
            self.sign_penalty_tx(fee_rate.map(FeeRate::from_sat_per_kwu))?;
        }
        Ok(())
    }

    /// What a restart does to a trade whose deposit transaction the trader has not signed: the
    /// trade is abandoned and its coins are released, because its nonces are lost and its setup
    /// cannot continue. Returns whether the trade was abandoned.
    pub fn abandon_after_restart(&mut self, wallet: &mut dyn ProtocolWalletApi) -> Result<bool> {
        if self.state != TradeState::Init {
            return Ok(false);
        }
        self.abandon(wallet)?;
        Ok(true)
    }
}
