use bdk_wallet::bitcoin::address::NetworkUnchecked;
use bdk_wallet::bitcoin::hashes::Hash as _;
use bdk_wallet::bitcoin::{
    Address, Amount, Psbt, TapSighash, Transaction, Txid, XOnlyPublicKey, consensus,
};
use musig2::PubNonce;
use musig2::secp::{MaybeScalar, Point, Scalar};
use prost::UnknownEnumValue;
use protocol::receiver::Receiver;
use protocol::storage::{ByRef, ByVal};
use protocol::trade_model::{
    ContractualTxids, ExchangedAddresses, ExchangedNonces, ExchangedSigs, Role,
};
use tonic::{Result, Status};

use crate::pb::musigrpc::{
    self, NonceSharesMessage, PartialSignaturesMessage, ReceiverAddressAndAmount,
};

/// Serializers that stand in for a secret's value.
///
/// `handle_request` logs every request (and response) as JSON, so any field carrying a password,
/// passphrase, seed phrase or private key share is wired up to one of these in `build.rs`. The
/// field still shows up in the log — so it's obvious whether the client sent one — but never
/// its content.
pub(crate) mod redact {
    use serde::Serializer;

    const PLACEHOLDER: &str = "<redacted>";

    pub fn string<S: Serializer>(_value: &String, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(PLACEHOLDER)
    }

    pub fn string_vec<S: Serializer>(value: &[String], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(value.iter().map(|_| PLACEHOLDER))
    }

    /// For an optional secret: the log shows whether the field is set.
    #[expect(
        clippy::ref_option,
        reason = "the signature that `serialize_with` requires"
    )]
    pub fn opt_bytes<S: Serializer>(
        value: &Option<Vec<u8>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(_) => serializer.serialize_str(PLACEHOLDER),
            None => serializer.serialize_none(),
        }
    }
}

#[cfg(test)]
mod tests {
    use bdk_wallet::serde_json;

    use crate::pb::musigrpc::{CloseTradeRequest, CloseTradeResponse, SwapTxSignatureResponse};

    /// K5 (F7): the form in which requests and responses are written to the log contains no
    /// private key share, and still shows whether one is set.
    #[test]
    fn k5_logged_messages_contain_no_private_key_share() {
        let share = vec![0xAB; 32];
        let logged = [
            serde_json::to_string(&CloseTradeRequest {
                my_output_peers_prv_key_share: Some(share.clone()),
                ..CloseTradeRequest::default()
            }),
            serde_json::to_string(&SwapTxSignatureResponse {
                peer_output_prv_key_share: Some(share.clone()),
            }),
            serde_json::to_string(&CloseTradeResponse {
                peer_output_prv_key_share: Some(share),
                ..CloseTradeResponse::default()
            }),
        ];
        for message in logged {
            let message = message.expect("serializable");
            // The share in the forms that the log has used: base64 and hex.
            assert!(!message.contains("q6ur"), "{message}");
            assert!(!message.to_lowercase().contains("abab"), "{message}");
            assert!(
                message.contains("PrvKeyShare\":\"<redacted>\""),
                "{message}"
            );
        }
        let unset = serde_json::to_string(&CloseTradeRequest::default()).expect("serializable");
        assert!(
            unset.contains("\"myOutputPeersPrvKeyShare\":null"),
            "{unset}"
        );
    }
}

pub trait CheckInSignedRange: Sized {
    /// # Errors
    /// Will return `Err` if casting to a signed integer of the same size would overflow
    fn check_in_signed_range(self) -> Result<Self>;
}

impl CheckInSignedRange for u64 {
    fn check_in_signed_range(self) -> Result<Self> {
        if self > i64::MAX as Self {
            return Err(Status::invalid_argument(format!("out of signed range: {self}")));
        }
        Ok(self)
    }
}

pub trait TryProtoInto<T> {
    /// # Errors
    /// Will return `Err` if conversion from proto fails
    fn try_proto_into(self) -> Result<T>;
}

macro_rules! impl_try_proto_into_for_slice {
    ($into_type:ty, $try_from_fn:expr, $err_msg:literal) => {
        impl TryProtoInto<$into_type> for &[u8] {
            fn try_proto_into(self) -> Result<$into_type> {
                $try_from_fn(self).map_err(|e| {
                    Status::invalid_argument(format!("could not decode {}: {e}", $err_msg))
                })
            }
        }
    };
}

impl_try_proto_into_for_slice!(Point, Point::try_from, "nonzero point");
impl_try_proto_into_for_slice!(PubNonce, PubNonce::try_from, "pub nonce");
impl_try_proto_into_for_slice!(Scalar, Scalar::try_from, "nonzero scalar");
impl_try_proto_into_for_slice!(MaybeScalar, MaybeScalar::try_from, "scalar");

impl_try_proto_into_for_slice!(Txid, Txid::from_slice, "txid");
impl_try_proto_into_for_slice!(TapSighash, TapSighash::from_slice, "sighash");
impl_try_proto_into_for_slice!(XOnlyPublicKey, XOnlyPublicKey::from_slice, "x-only pubkey");
impl_try_proto_into_for_slice!(Transaction, consensus::deserialize, "transaction");
impl_try_proto_into_for_slice!(Psbt, Psbt::deserialize, "PSBT");

impl TryProtoInto<Role> for i32 {
    fn try_proto_into(self) -> Result<Role> {
        TryInto::<musigrpc::Role>::try_into(self)
            .map_err(|UnknownEnumValue(i)| Status::out_of_range(format!("unknown enum value: {i}")))
            .map(Into::into)
    }
}

impl TryProtoInto<Address<NetworkUnchecked>> for &str {
    fn try_proto_into(self) -> Result<Address<NetworkUnchecked>> {
        self.parse::<Address<_>>()
            .map_err(|e| Status::invalid_argument(format!("could not parse address: {e}")))
    }
}

impl TryProtoInto<Receiver<NetworkUnchecked>> for ReceiverAddressAndAmount {
    fn try_proto_into(self) -> Result<Receiver<NetworkUnchecked>> {
        Ok(Receiver {
            address: self.address.try_proto_into()?,
            amount: Amount::from_sat(self.amount.check_in_signed_range()?),
        })
    }
}

impl<T> TryProtoInto<T> for Vec<u8> where for<'a> &'a [u8]: TryProtoInto<T> {
    fn try_proto_into(self) -> Result<T> { (&self[..]).try_proto_into() }
}

impl<T> TryProtoInto<T> for String where for<'a> &'a str: TryProtoInto<T> {
    fn try_proto_into(self) -> Result<T> { (&self[..]).try_proto_into() }
}

impl<T, S: TryProtoInto<T>> TryProtoInto<Option<T>> for Option<S> {
    fn try_proto_into(self) -> Result<Option<T>> {
        Ok(match self {
            None => None,
            Some(x) => Some(x.try_proto_into()?)
        })
    }
}

type SentAddressesNoncesPair<'a> = (ExchangedAddresses<'a, ByRef>, ExchangedNonces<'a, ByRef>);

type ReceivedAddressesNoncesPair<'a> = (ExchangedAddresses<'a, ByVal, NetworkUnchecked>, ExchangedNonces<'a, ByVal>);

impl<'a> TryProtoInto<ReceivedAddressesNoncesPair<'a>> for NonceSharesMessage {
    fn try_proto_into(self) -> Result<ReceivedAddressesNoncesPair<'a>> {
        Ok((ExchangedAddresses {
            warning_tx_fee_bump:
            self.warning_tx_fee_bump_address.try_proto_into()?,
            redirect_tx_fee_bump:
            self.redirect_tx_fee_bump_address.try_proto_into()?,
            claim_tx_payout:
            self.claim_tx_payout_address.try_proto_into()?,
        }, ExchangedNonces {
            swap_tx_input:
            self.swap_tx_input_nonce_share.try_proto_into()?,
            buyers_warning_tx_buyer_input:
            self.buyers_warning_tx_buyer_input_nonce_share.try_proto_into()?,
            buyers_warning_tx_seller_input:
            self.buyers_warning_tx_seller_input_nonce_share.try_proto_into()?,
            sellers_warning_tx_buyer_input:
            self.sellers_warning_tx_buyer_input_nonce_share.try_proto_into()?,
            sellers_warning_tx_seller_input:
            self.sellers_warning_tx_seller_input_nonce_share.try_proto_into()?,
            buyers_redirect_tx_input:
            self.buyers_redirect_tx_input_nonce_share.try_proto_into()?,
            sellers_redirect_tx_input:
            self.sellers_redirect_tx_input_nonce_share.try_proto_into()?,
        }))
    }
}

impl<'a> TryProtoInto<ExchangedSigs<'a, ByVal>> for PartialSignaturesMessage {
    fn try_proto_into(self) -> Result<ExchangedSigs<'a, ByVal>> {
        Ok(ExchangedSigs {
            peers_warning_tx_buyer_input_partial_signature:
            self.peers_warning_tx_buyer_input_partial_signature.try_proto_into()?,
            peers_warning_tx_seller_input_partial_signature:
            self.peers_warning_tx_seller_input_partial_signature.try_proto_into()?,
            peers_redirect_tx_input_partial_signature:
            self.peers_redirect_tx_input_partial_signature.try_proto_into()?,
            swap_tx_input_partial_signature:
            self.swap_tx_input_partial_signature.try_proto_into()?,
            swap_tx_input_sighash:
            self.swap_tx_input_sighash.try_proto_into()?,
            contractual_txids:
            None, // ignore any contract-forming txids passed by the client
        })
    }
}

impl From<musigrpc::Role> for Role {
    fn from(value: musigrpc::Role) -> Self {
        match value {
            musigrpc::Role::SellerAsMaker => Self::SellerAsMaker,
            musigrpc::Role::SellerAsTaker => Self::SellerAsTaker,
            musigrpc::Role::BuyerAsMaker => Self::BuyerAsMaker,
            musigrpc::Role::BuyerAsTaker => Self::BuyerAsTaker
        }
    }
}

impl From<SentAddressesNoncesPair<'_>> for NonceSharesMessage {
    fn from((addresses, nonces): SentAddressesNoncesPair) -> Self {
        Self {
            // Use default value for the PSBT & redirection amount fields. TODO: A little hacky; consider refactoring proto.
            half_deposit_psbt: Vec::default(),
            redirection_amount_msat: 0,
            // Addresses...
            warning_tx_fee_bump_address: addresses.warning_tx_fee_bump.to_string(),
            redirect_tx_fee_bump_address: addresses.redirect_tx_fee_bump.to_string(),
            claim_tx_payout_address: addresses.claim_tx_payout.to_string(),
            // Actual nonce shares...
            swap_tx_input_nonce_share:
            nonces.swap_tx_input.serialize().into(),
            buyers_warning_tx_buyer_input_nonce_share:
            nonces.buyers_warning_tx_buyer_input.serialize().into(),
            buyers_warning_tx_seller_input_nonce_share:
            nonces.buyers_warning_tx_seller_input.serialize().into(),
            sellers_warning_tx_buyer_input_nonce_share:
            nonces.sellers_warning_tx_buyer_input.serialize().into(),
            sellers_warning_tx_seller_input_nonce_share:
            nonces.sellers_warning_tx_seller_input.serialize().into(),
            buyers_redirect_tx_input_nonce_share:
            nonces.buyers_redirect_tx_input.serialize().into(),
            sellers_redirect_tx_input_nonce_share:
            nonces.sellers_redirect_tx_input.serialize().into(),
        }
    }
}

impl From<ContractualTxids> for musigrpc::ContractualTxIds {
    fn from(value: ContractualTxids) -> Self {
        Self {
            deposit_tx_id: value.deposit.to_string(),
            buyers_warning_tx_id: value.buyers_warning.to_string(),
            sellers_warning_tx_id: value.sellers_warning.to_string(),
            buyers_redirect_tx_id: value.buyers_redirect.to_string(),
            sellers_redirect_tx_id: value.sellers_redirect.to_string(),
        }
    }
}

impl From<ExchangedSigs<'_, ByRef>> for PartialSignaturesMessage {
    fn from(value: ExchangedSigs<ByRef>) -> Self {
        Self {
            peers_warning_tx_buyer_input_partial_signature:
            value.peers_warning_tx_buyer_input_partial_signature.serialize().into(),
            peers_warning_tx_seller_input_partial_signature:
            value.peers_warning_tx_seller_input_partial_signature.serialize().into(),
            peers_redirect_tx_input_partial_signature:
            value.peers_redirect_tx_input_partial_signature.serialize().into(),
            swap_tx_input_partial_signature:
            value.swap_tx_input_partial_signature.map(|s| s.serialize().into()),
            swap_tx_input_sighash:
            value.swap_tx_input_sighash.map(|s| s.as_byte_array().into()),
            contractual_tx_ids:
            value.contractual_txids.map(ContractualTxids::into),
        }
    }
}
