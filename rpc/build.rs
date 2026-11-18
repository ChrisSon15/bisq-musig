use std::borrow::Cow;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        // Add Serde serialization for musigrpc request types...
        .serde_serialized_types(&[
            "ReceiverAddressAndAmount", "PartialSignaturesRequest", "DepositTxSignatureRequest",
            "PublishDepositTxRequest", "SubscribeTxConfirmationStatusRequest", "ContractualTxIds",
            "CustomPayoutPsbtRequest", "AbandonTradeRequest", "AbandonTradeResponse",
            "PublishWarningTxRequest", "SubscribeTradeEventsRequest", "TradeEvent"
        ])
        .serde_serialized_type("PubKeySharesRequest", &[
            enum_field("myRole", "Role")
        ])
        .serde_serialized_type("NonceSharesRequest", &[
            base64("buyerOutputPeersPubKeyShare"), base64("sellerOutputPeersPubKeyShare"),
            base64("peersMultisigScriptKey")
        ])
        .serde_serialized_type("NonceSharesMessage", &[
            base64("halfDepositPsbt"), base64("swapTxInputNonceShare"),
            base64("buyersWarningTxBuyerInputNonceShare"), base64("buyersWarningTxSellerInputNonceShare"),
            base64("sellersWarningTxBuyerInputNonceShare"), base64("sellersWarningTxSellerInputNonceShare"),
            base64("buyersRedirectTxInputNonceShare"), base64("sellersRedirectTxInputNonceShare")
        ])
        .serde_serialized_type("PartialSignaturesMessage", &[
            base64("peersWarningTxBuyerInputPartialSignature"), base64("peersWarningTxSellerInputPartialSignature"),
            base64("peersRedirectTxInputPartialSignature"),
            opt_base64("swapTxInputPartialSignature"), opt_base64("swapTxInputSighash")
        ])
        .serde_serialized_type("DepositPsbt", &[
            base64("depositPsbt")
        ])
        .serde_serialized_type("SwapTxSignatureRequest", &[
            base64("swapTxInputPeersPartialSignature")
        ])
        // A private key share is a secret: with it and his own share, its receiver can spend
        // the payout. Its value is not written to the log.
        .serde_serialized_type("CloseTradeRequest", &[
            redacted_opt_bytes("myOutputPeersPrvKeyShare"), opt_hex("swapTx")
        ])
        .serde_serialized_type("CustomCloseTradeRequest", &[
            base64("peersCustomPayoutPsbt")
        ])
        .serde_serialized_enum("Role")

        // Add Serde serialization for musigrpc response types...
        .serde_serialized_type("PubKeySharesResponse", &[
            base64("buyerOutputPubKeyShare"), base64("sellerOutputPubKeyShare"),
            base64("multisigScriptKey")
        ])
        .serde_serialized_type("TxConfirmationStatus", &[
            hex("tx")
        ])
        .serde_serialized_type("SwapTxSignatureResponse", &[
            redacted_opt_bytes("peerOutputPrvKeyShare")
        ])
        .serde_serialized_type("CloseTradeResponse", &[
            redacted_opt_bytes("peerOutputPrvKeyShare"), opt_hex("swapTx"), opt_hex("penaltyTx")
        ])
        .serde_serialized_type("CustomPayoutPsbt", &[
            base64("psbt")
        ])
        .serde_serialized_type("CustomCloseTradeResponse", &[
            hex("customPayoutTx")
        ])
        .serde_serialized_type("PublishWarningTxResponse", &[
            hex("warningTx")
        ])

        // ...and for the bisq2-facing `wallet` (BMPWallet) types.
        .pipe(configure_bmp_wallet_serde)

        // Now compile all the protos...
        .compile_protos(
            &[
                "src/main/proto/rpc.proto",
                "src/main/proto/bmp_wallet.proto",
            ],
            &["src/main/proto"],
        )?;
    Ok(())
}

/// Serde config for the `wallet` package declared in `bmp_wallet.proto`.
///
/// Every path is fully qualified: unqualified paths match on any suffix, so a bare name would
/// silently apply the field attributes to a message of the same name in another package.
///
/// Secrets are redacted rather than skipped, so the request log still records that a field was
/// present without ever writing out its value.
fn configure_bmp_wallet_serde(builder: tonic_prost_build::Builder) -> tonic_prost_build::Builder {
    builder
        .serde_serialized_types(&[
            ".wallet.IsWalletReadyRequest",
            ".wallet.IsWalletReadyResponse",
            ".wallet.GetUnusedAddressRequest",
            ".wallet.GetUnusedAddressResponse",
            ".wallet.GetWalletAddressesRequest",
            ".wallet.GetWalletAddressesResponse",
            ".wallet.ListTransactionsRequest",
            ".wallet.ListTransactionsResponse",
            ".wallet.ListUtxosRequest",
            ".wallet.ListUtxosResponse",
            ".wallet.SendToAddressResponse",
            ".wallet.GetBalanceRequest",
            ".wallet.GetBalanceResponse",
            ".wallet.OpenOrCreateWalletResponse",
            ".wallet.ChangePasswordResponse",
            ".wallet.ReleaseUtxoRequest",
            ".wallet.ReleaseUtxoResponse",
            ".wallet.CloseRequest",
            ".wallet.CloseResponse",
            ".wallet.PubAddressInfo",
            ".wallet.TransactionInput",
            ".wallet.TransactionOutput",
            ".wallet.Transaction",
            ".wallet.Utxo",
        ])
        .serde_serialized_type(
            ".wallet.SendToAddressRequest",
            &[redacted_string("passphrase")],
        )
        .serde_serialized_type(
            ".wallet.GetSeedWordsRequest",
            &[redacted_string("password")],
        )
        .serde_serialized_type(
            ".wallet.GetSeedWordsResponse",
            &[redacted_string_vec("seedWords")],
        )
        .serde_serialized_type(
            ".wallet.OpenOrCreateWalletRequest",
            &[redacted_string("password")],
        )
        .serde_serialized_type(
            ".wallet.ChangePasswordRequest",
            &[redacted_string("oldPassword"), redacted_string("newPassword")],
        )
}

type CustomField<'a> = (&'a str, Cow<'static, str>);

const fn hex(field: &str) -> CustomField<'_> {
    (field, Cow::Borrowed("#[serde_as(as = \"::serde_with::hex::Hex\")]"))
}

const fn base64(field: &str) -> CustomField<'_> {
    (field, Cow::Borrowed("#[serde_as(as = \"::serde_with::base64::Base64\")]"))
}

const fn opt_hex(field: &str) -> CustomField<'_> {
    (field, Cow::Borrowed("#[serde_as(as = \"::core::option::Option<::serde_with::hex::Hex>\")]"))
}

const fn opt_base64(field: &str) -> CustomField<'_> {
    (field, Cow::Borrowed("#[serde_as(as = \"::core::option::Option<::serde_with::base64::Base64>\")]"))
}

fn enum_field<'a>(field: &'a str, type_name: &'_ str) -> CustomField<'a> {
    (field, Cow::Owned(format!("#[serde_as(as = \"::serde_with::TryFromInto<{type_name}>\")]")))
}

const fn redacted_string(field: &str) -> CustomField<'_> {
    (
        field,
        Cow::Borrowed("#[serde(serialize_with = \"crate::pb::convert::redact::string\")]"),
    )
}

const fn redacted_opt_bytes(field: &str) -> CustomField<'_> {
    (
        field,
        Cow::Borrowed("#[serde(serialize_with = \"crate::pb::convert::redact::opt_bytes\")]"),
    )
}

const fn redacted_string_vec(field: &str) -> CustomField<'_> {
    (
        field,
        Cow::Borrowed("#[serde(serialize_with = \"crate::pb::convert::redact::string_vec\")]"),
    )
}

trait BuilderEx {
    fn serde_serialized_enum(self, path: &str) -> Self;

    fn serde_serialized_type(self, path: &str, custom_fields: &[CustomField]) -> Self;

    fn serde_serialized_types(mut self, paths: &[&str]) -> Self where Self: Sized {
        for &path in paths {
            self = self.serde_serialized_type(path, &[]);
        }
        self
    }

    /// Applies a configuration step written as a free function, so long chains can be broken up.
    fn pipe(self, step: impl FnOnce(Self) -> Self) -> Self
    where
        Self: Sized,
    {
        step(self)
    }
}

impl BuilderEx for tonic_prost_build::Builder {
    fn serde_serialized_enum(self, path: &str) -> Self {
        self.enum_attribute(path, "#[derive(::serde::Serialize)]")
            .enum_attribute(path, "#[serde(rename_all = \"SCREAMING_SNAKE_CASE\")]")
    }

    fn serde_serialized_type(mut self, path: &str, custom_fields: &[CustomField]) -> Self {
        self = self
            .type_attribute(path, "#[::serde_with::serde_as]")
            .type_attribute(path, "#[derive(::serde::Serialize)]")
            .type_attribute(path, "#[serde(rename_all = \"camelCase\")]");
        for (field, attribute) in custom_fields {
            self = self.field_attribute(format!("{path}.{field}"), attribute);
        }
        self
    }
}
