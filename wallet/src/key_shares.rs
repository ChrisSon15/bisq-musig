//! Derivation of the secret key shares of a trade from the wallet seed.
//!
//! The rule is specified in `docs/specifications/trade/key-derivation.md`. It must never change,
//! because every open trade depends on it: after a restart or a restore of the wallet, the same
//! seed and the same trade ID must give the same key shares again.

use bdk_wallet::bitcoin::NetworkKind;
use bdk_wallet::bitcoin::bip32::{ChildNumber, Xpriv};
use bdk_wallet::bitcoin::hashes::{Hash as _, HashEngine as _, sha256};
use secp::{MaybeScalar, Scalar};
use zeroize::Zeroizing;

use crate::error::{Result, WalletErrorKind};
use crate::protocol_wallet_api::LIBSECP256K1_CTX;

/// The tag of the tagged hash that gives a key share.
const TAG: &[u8] = b"BisqMuSig/key-share";
/// The label of the trader's share of the key of the buyer's payout.
const BUYER_PAYOUT_LABEL: u8 = 0x00;
/// The label of the trader's share of the key of the seller's payout.
const SELLER_PAYOUT_LABEL: u8 = 0x01;
/// The hardened child of the BIP86 account key that is reserved for the trade root secret. The
/// BIP86 wallet uses only the unhardened children 0 (receive) and 1 (change).
const TRADE_ROOT_CHILD: u32 = 2;

/// The two secret key shares that one trader holds for one trade.
///
/// The shares do not depend on the role of the trader: a buyer and a seller with the same seed
/// and the same trade ID get the same two shares.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TradeKeyShares {
    /// The trader's share of the key of the buyer's payout.
    pub buyer_payout: Scalar,
    /// The trader's share of the key of the seller's payout.
    pub seller_payout: Scalar,
}

/// Derives the key shares of the trade `trade_id` from the master key of the wallet.
///
/// The trade ID is used byte for byte. A trade ID must never be used for two trades of one
/// wallet, because the second trade would get the key shares of the first one.
pub fn derive_trade_key_shares(master_key: &Xpriv, trade_id: &str) -> Result<TradeKeyShares> {
    let coin_type = match master_key.network {
        NetworkKind::Main => 0,
        NetworkKind::Test => 1,
    };
    let path = [86, coin_type, 0, TRADE_ROOT_CHILD]
        .map(|index| ChildNumber::from_hardened_idx(index).expect("index is below 2^31"));
    let trade_root = master_key.derive_priv(&*LIBSECP256K1_CTX, &path)?;
    let trade_root_secret = Zeroizing::new(trade_root.private_key.secret_bytes());

    let trade_id_length = u32::try_from(trade_id.len())
        .map_err(|_| WalletErrorKind::InvalidTradeId("it is too long"))?;
    let key_share = |label: u8| {
        let tag_hash = sha256::Hash::hash(TAG);
        let mut engine = sha256::Hash::engine();
        engine.input(tag_hash.as_byte_array());
        engine.input(tag_hash.as_byte_array());
        engine.input(&trade_root_secret[..]);
        engine.input(&trade_id_length.to_be_bytes());
        engine.input(trade_id.as_bytes());
        engine.input(&[label]);
        let hash = Zeroizing::new(sha256::Hash::from_engine(engine).to_byte_array());
        match MaybeScalar::reduce_from(&hash) {
            MaybeScalar::Valid(key_share) => Ok(key_share),
            // The probability of this case is about 2^-256.
            MaybeScalar::Zero => Err(WalletErrorKind::InvalidTradeId(
                "it gives a key share of zero",
            )),
        }
    };
    Ok(TradeKeyShares {
        buyer_payout: key_share(BUYER_PAYOUT_LABEL)?,
        seller_payout: key_share(SELLER_PAYOUT_LABEL)?,
    })
}

#[cfg(test)]
mod tests {
    use bdk_wallet::bitcoin::Network;
    use bdk_wallet::bitcoin::hex::DisplayHex as _;
    use bdk_wallet::keys::bip39::Mnemonic;

    use super::*;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon \
                            abandon abandon abandon about";

    fn master_key(network: Network) -> Xpriv {
        let seed = Mnemonic::parse_normalized(MNEMONIC)
            .expect("valid mnemonic")
            .to_seed("");
        Xpriv::new_master(network, &seed).expect("valid seed")
    }

    fn hex(key_share: Scalar) -> (String, String) {
        (
            key_share.serialize().to_lower_hex_string(),
            key_share.base_point_mul().serialize().to_lower_hex_string(),
        )
    }

    /// The test vectors of `docs/specifications/trade/key-derivation.md`: trade ID, secret and
    /// public key share with label `0x00`, secret and public key share with label `0x01`.
    type Vector = (&'static str, [&'static str; 2], [&'static str; 2]);

    const COIN_TYPE_1_VECTORS: [Vector; 2] = [
        (
            "trade-1",
            [
                "b2791aa0c44ba3f99274c5faffd9a5ecfb2a04081b811d81a39b5f1ca63a9c86",
                "0365af31fa475be070b752da8652cfd92cbe6c5d80070a1ddfedb821ff2435a576",
            ],
            [
                "5ad667b8870bf171342cc66d1048ab0f8aa563bb9a22b5e6a5700e839ca85770",
                "02eeb60b61906388346cf8caa2fe5a79596dd3fac1e047f79c9ae8cf577bc361ba",
            ],
        ),
        (
            "trade-2",
            [
                "83ccc10bc994823b37de146604a3e1ae78ae24340b1692f0164ceaca92414a8a",
                "03a672e62c51e14078f3508c53d7b40829febf3aacc081b4adfb76757ce874a115",
            ],
            [
                "c7fa6364f66f22102b42b4017aef6b03bbad42b1a20d08330fa0d28f8471155e",
                "03fd2089b1e906ac40656964072a43a488171724a59ab1d0e2e7ecafd5279d9460",
            ],
        ),
    ];

    const COIN_TYPE_0_VECTORS: [Vector; 2] = [
        (
            "trade-1",
            [
                "9cc8073b223e4cc4dbb5223da35ee5c2e79ed293a586cbb3a2e52bd2346fe7e4",
                "024efb07047fb747611c81bbd2cd5706282f7ea71a051836a2475fbba28d74478f",
            ],
            [
                "a92f8710f192377d086ae52ce0145cf98a3a43a9a6d667803eba9aaf43e63fa1",
                "021ceeaf87477ace942b0e0eacc8c2fabe39b04a5cc30729d5a347062f95a1a3a5",
            ],
        ),
        (
            "trade-2",
            [
                "ebf246e7bd1d169bf652110616a5fa29e6e8e22e2f06feca9adf381d7e684674",
                "035a8189e2549b1de0be14462952c11bf72977f9cab96726f3748219d3d7187c5c",
            ],
            [
                "e8cbd0e40b2a3e95ff5810057730d5ed7803bb644737ffa080ed4fce8688c38a",
                "0383d38dfcea5820febbb13c5594d49d480d55904bcd4a1545e5c3f5cd3605e250",
            ],
        ),
    ];

    #[track_caller]
    fn assert_vectors(master_key: &Xpriv, vectors: &[Vector]) {
        for (trade_id, buyer_payout, seller_payout) in vectors {
            let key_shares = derive_trade_key_shares(master_key, trade_id).expect("valid");
            let (secret, public) = hex(key_shares.buyer_payout);
            assert_eq!([secret.as_str(), public.as_str()], *buyer_payout);
            let (secret, public) = hex(key_shares.seller_payout);
            assert_eq!([secret.as_str(), public.as_str()], *seller_payout);
        }
    }

    /// H9: the implementation reproduces the test vectors of the specification. Testnet, signet
    /// and regtest use coin type 1, mainnet uses coin type 0.
    #[test]
    fn h9_test_vectors_of_the_specification() {
        for network in [Network::Regtest, Network::Testnet, Network::Signet] {
            assert_vectors(&master_key(network), &COIN_TYPE_1_VECTORS);
        }
        assert_vectors(&master_key(Network::Bitcoin), &COIN_TYPE_0_VECTORS);
    }

    /// The trade ID is used byte for byte: case and white space change the key shares.
    #[test]
    fn trade_id_is_not_normalised() {
        let master_key = master_key(Network::Regtest);
        let key_shares = |trade_id| {
            derive_trade_key_shares(&master_key, trade_id)
                .expect("valid")
                .buyer_payout
        };
        let reference = key_shares("trade-1");
        for other in [
            "Trade-1",
            "trade-1 ",
            " trade-1",
            "trade-1\n",
            "trade-11",
            "",
        ] {
            assert_ne!(key_shares(other), reference, "{other:?}");
        }
        assert_eq!(key_shares("trade-1"), reference);
    }

    /// The length field separates the trade ID from the label: a trade ID that ends with the
    /// label byte of another share gives other key shares.
    #[test]
    fn trade_id_and_label_cannot_be_confused() {
        let master_key = master_key(Network::Regtest);
        let short = derive_trade_key_shares(&master_key, "a").expect("valid");
        let long = derive_trade_key_shares(&master_key, "a\u{0}").expect("valid");
        assert_ne!(short.seller_payout, long.buyer_payout);
        assert_ne!(short.buyer_payout, long.buyer_payout);
        assert_ne!(short.buyer_payout, short.seller_payout);
    }
}
