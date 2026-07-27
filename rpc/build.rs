use std::borrow::Cow;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        // Add Serde serialization for walletrpc request types...
        .serde_serialized_types(&[
            "WalletBalanceRequest", "NewAddressRequest", "ListUnspentRequest"
        ])
        .serde_serialized_type("ConfRequest", &[
            rev_hex("txId")
        ])

        // Add Serde serialization for walletrpc response types...
        .serde_serialized_types(&["WalletBalanceResponse", "NewAddressResponse", "ListUnspentResponse"])
        .serde_serialized_type("TransactionOutput", &[
            rev_hex("txId"), hex("scriptPubKey")
        ])
        .serde_serialized_type("ConfEvent", &[
            opt_hex("rawTx"), enum_field("confidenceType", "ConfidenceType")
        ])
        .serde_serialized_type("ConfirmationBlockTime", &[
            rev_hex("blockHash")
        ])
        .serde_serialized_enum("ConfidenceType")

        // Add Serde serialization for musigrpc request types...
        .serde_serialized_types(&[
            "ReceiverAddressAndAmount", "PartialSignaturesRequest", "DepositTxSignatureRequest",
            "PublishDepositTxRequest", "SubscribeTxConfirmationStatusRequest", "ContractualTxIds",
            "CustomPayoutPsbtRequest"
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
            base64("buyersRedirectTxInputNonceShare"), base64("sellersRedirectTxInputNonceShare"),
            base64("buyersClaimTxInputNonceShare"), base64("sellersClaimTxInputNonceShare")
        ])
        .serde_serialized_type("PartialSignaturesMessage", &[
            base64("peersWarningTxBuyerInputPartialSignature"), base64("peersWarningTxSellerInputPartialSignature"),
            base64("peersRedirectTxInputPartialSignature"), base64("peersClaimTxInputPartialSignature"),
            opt_base64("swapTxInputPartialSignature"), opt_base64("swapTxInputSighash")
        ])
        .serde_serialized_type("DepositPsbt", &[
            base64("depositPsbt")
        ])
        .serde_serialized_type("SwapTxSignatureRequest", &[
            base64("swapTxInputPeersPartialSignature")
        ])
        .serde_serialized_type("CloseTradeRequest", &[
            opt_base64("myOutputPeersPrvKeyShare"), opt_hex("swapTx")
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
            hex("swapTx"), base64("peerOutputPrvKeyShare")
        ])
        .serde_serialized_type("CloseTradeResponse", &[
            base64("peerOutputPrvKeyShare")
        ])
        .serde_serialized_type("CustomPayoutPsbt", &[
            base64("psbt")
        ])
        .serde_serialized_type("CustomCloseTradeResponse", &[
            hex("customPayoutTx")
        ])

        // Now compile all the protos...
        .compile_protos(
            &[
                "src/main/proto/rpc.proto",
                "src/main/proto/wallet.proto",
                "src/main/proto/bmp_protocol.proto",
            ],
            &["src/main/proto"],
        )?;
    Ok(())
}

type CustomField<'a> = (&'a str, Cow<'static, str>);

const fn hex(field: &str) -> CustomField<'_> {
    (field, Cow::Borrowed("#[serde_as(as = \"::serde_with::hex::Hex\")]"))
}

const fn base64(field: &str) -> CustomField<'_> {
    (field, Cow::Borrowed("#[serde_as(as = \"::serde_with::base64::Base64\")]"))
}

const fn rev_hex(field: &str) -> CustomField<'_> {
    (field, Cow::Borrowed("#[serde_as(as = \"crate::pb::convert::hex::ByteReversedHex\")]"))
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

trait BuilderEx {
    fn serde_serialized_enum(self, path: &str) -> Self;

    fn serde_serialized_type(self, path: &str, custom_fields: &[CustomField]) -> Self;

    fn serde_serialized_types(mut self, paths: &[&str]) -> Self where Self: Sized {
        for &path in paths {
            self = self.serde_serialized_type(path, &[]);
        }
        self
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
