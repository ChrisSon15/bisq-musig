use bdk_wallet::bitcoin::hashes::Hash as _;
use bdk_wallet::bitcoin::key::TweakedPublicKey;
use bdk_wallet::bitcoin::taproot::Signature;
use bdk_wallet::bitcoin::{Address, Network, PublicKey, TapNodeHash, TapSighash};
use musig2::adaptor::AdaptorSignature;
use musig2::secp::{MaybePoint, MaybeScalar, Point, Scalar};
use musig2::{
    AggNonce, KeyAggContext, LiftedSignature, NonceSeed, PartialSignature, PubNonce, SecNonce,
    SecNonceBuilder,
};
use thiserror::Error;

pub struct KeyPair {
    pub_key: Point,
    prv_key: Option<Scalar>,
}

impl KeyPair {
    pub const fn pub_key(&self) -> &Point { &self.pub_key }

    pub fn prv_key(&self) -> Result<&Scalar> {
        self.prv_key.as_ref().ok_or(MultisigErrorKind::MissingPrvKey)
    }

    const fn from_public(pub_key: Point) -> Self {
        Self { pub_key, prv_key: None }
    }

    fn from_private(prv_key: Scalar) -> Self {
        Self { pub_key: prv_key.base_point_mul(), prv_key: Some(prv_key) }
    }

    fn random<R: rand::RngCore + rand::CryptoRng>(rng: &mut R) -> Self {
        Self::from_private(Scalar::random(rng))
    }

    fn set_prv_key(&mut self, prv_key: Scalar) -> Result<&Scalar> {
        if self.pub_key != prv_key.base_point_mul() {
            return Err(MultisigErrorKind::MismatchedKeyPair);
        }
        Ok(self.prv_key.insert(prv_key))
    }
}

struct NoncePair {
    pub_nonce: PubNonce,
    sec_nonce: Option<SecNonce>,
}

impl NoncePair {
    fn new(nonce_seed: impl Into<NonceSeed>, seckey: Scalar, aggregated_pubkey: Point) -> Self {
        let sec_nonce = SecNonceBuilder::from_seckey(nonce_seed, seckey)
            .with_aggregated_pubkey(aggregated_pubkey)
            .build();
        Self { pub_nonce: sec_nonce.public_nonce(), sec_nonce: Some(sec_nonce) }
    }
}

#[derive(Default)]
pub struct KeyCtx {
    my_key_share: Option<KeyPair>,
    peers_key_share: Option<KeyPair>,
    aggregated_key: Option<KeyPair>,
    key_agg_ctx: Option<KeyAggContext>,
}

impl KeyCtx {
    pub fn init_my_key_share(&mut self) -> &KeyPair {
        // TODO: Consider making the RNG configurable, to aid unit testing. (Also, we may not
        //  necessarily want to use a nondeterministic random key share):
        self.my_key_share.get_or_insert_with(|| KeyPair::random(&mut rand::rng()))
    }

    pub fn my_key_share(&self) -> Result<&KeyPair> {
        self.my_key_share.as_ref().ok_or(MultisigErrorKind::MissingKeyShare)
    }

    pub fn set_peers_pub_key(&mut self, pub_key: Point) -> &KeyPair {
        self.peers_key_share.get_or_insert(KeyPair::from_public(pub_key))
    }

    pub fn peers_key_share(&self) -> Result<&KeyPair> {
        self.peers_key_share.as_ref().ok_or(MultisigErrorKind::MissingKeyShare)
    }

    pub fn aggregated_key(&self) -> Result<&KeyPair> {
        self.aggregated_key.as_ref().ok_or(MultisigErrorKind::MissingAggPubKey)
    }

    fn key_shares(&self) -> Result<[&KeyPair; 2]> {
        let mut shares = [self.my_key_share()?, self.peers_key_share()?];
        shares.sort_by_key(|p| p.pub_key());
        Ok(shares)
    }

    pub fn aggregate_pub_key_shares(&mut self) -> Result<()> {
        let agg_ctx = KeyAggContext::new(self.key_shares()?.map(|p| *p.pub_key()))?;
        self.aggregated_key.get_or_insert(KeyPair::from_public(agg_ctx.aggregated_pubkey()));
        self.key_agg_ctx = Some(agg_ctx);
        Ok(())
    }

    fn prv_key_shares(&self) -> Result<[Scalar; 2]> {
        let shares = self.key_shares()?;
        Ok([*shares[0].prv_key()?, *shares[1].prv_key()?])
    }

    pub fn aggregate_prv_key_shares(&mut self) -> Result<&Scalar> {
        let prv_key_shares = self.prv_key_shares()?;
        let agg_ctx = self.key_agg_ctx.as_ref()
            .ok_or(MultisigErrorKind::MissingAggPubKey)?;
        let agg_key = self.aggregated_key.as_mut()
            .ok_or(MultisigErrorKind::MissingAggPubKey)?;
        agg_key.set_prv_key(agg_ctx.aggregated_seckey(prv_key_shares)?)
    }

    pub fn set_peers_prv_key(&mut self, prv_key: Scalar) -> Result<&Scalar> {
        self.peers_key_share.as_mut().ok_or(MultisigErrorKind::MissingKeyShare)?.set_prv_key(prv_key)
    }

    pub fn with_taproot_tweak(&self, merkle_root: Option<&TapNodeHash>) -> Result<TweakedKeyCtx> {
        let key_agg_ctx = self.compute_tweaked_key_agg_ctx(merkle_root)?;
        let my_prv_key = *self.my_key_share()?.prv_key()?;
        Ok(TweakedKeyCtx { my_prv_key, key_agg_ctx })
    }

    fn compute_tweaked_key_agg_ctx(&self, merkle_root: Option<&TapNodeHash>) -> Result<KeyAggContext> {
        let key_agg_ctx = self.key_agg_ctx.clone()
            .ok_or(MultisigErrorKind::MissingAggPubKey)?;
        Ok(if let Some(merkle_root) = merkle_root {
            key_agg_ctx.with_taproot_tweak(merkle_root.as_byte_array())?
        } else {
            key_agg_ctx.with_unspendable_taproot_tweak()?
        })
    }
}

#[derive(Clone)]
pub struct TweakedKeyCtx {
    my_prv_key: Scalar,
    key_agg_ctx: KeyAggContext,
}

impl TweakedKeyCtx {
    pub fn tweaked_public_key(&self) -> TweakedPublicKey {
        let pub_key: Point = self.key_agg_ctx.aggregated_pubkey();
        let pub_key = pub_key.to_public_key().into();

        // This is safe, as `self` can only be constructed with a Taproot tweak applied to its
        // inner KeyAggContext (performed via the 'musig2::secp' crate):
        TweakedPublicKey::dangerous_assume_tweaked(pub_key)
    }

    pub fn p2tr_address(&self, network: Network) -> Address {
        Address::p2tr_tweaked(self.tweaked_public_key(), network)
    }
}

#[derive(Default)]
pub struct SigCtx {
    tweaked_key_ctx: Option<TweakedKeyCtx>,
    adaptor_point: MaybePoint,
    my_nonce_pair_share: Option<NoncePair>,
    peers_nonce_share: Option<PubNonce>,
    aggregated_nonce: Option<AggNonce>,
    message: Option<TapSighash>,
    my_partial_sig: Option<PartialSignature>,
    peers_partial_sig: Option<PartialSignature>,
    aggregated_sig: Option<AdaptorSignature>,
}

impl SigCtx {
    pub fn tweaked_key_ctx(&self) -> Result<&TweakedKeyCtx> {
        self.tweaked_key_ctx.as_ref().ok_or(MultisigErrorKind::MissingAggPubKey)
    }

    pub fn set_tweaked_key_ctx(&mut self, tweaked_key_ctx: TweakedKeyCtx) -> &TweakedKeyCtx {
        self.tweaked_key_ctx.get_or_insert(tweaked_key_ctx)
    }

    pub fn my_nonce_share(&self) -> Result<&PubNonce> {
        Ok(&self.my_nonce_pair_share.as_ref().ok_or(MultisigErrorKind::MissingNonceShare)?.pub_nonce)
    }

    fn peers_nonce_share(&self) -> Result<&PubNonce> {
        self.peers_nonce_share.as_ref().ok_or(MultisigErrorKind::MissingNonceShare)
    }

    pub fn set_peers_nonce_share(&mut self, nonce_share: PubNonce) {
        self.peers_nonce_share.get_or_insert(nonce_share);
    }

    pub fn my_partial_sig(&self) -> Result<&PartialSignature> {
        self.my_partial_sig.as_ref().ok_or(MultisigErrorKind::MissingPartialSig)
    }

    fn peers_partial_sig(&self) -> Result<&PartialSignature> {
        self.peers_partial_sig.as_ref().ok_or(MultisigErrorKind::MissingPartialSig)
    }

    pub fn set_peers_partial_sig(&mut self, partial_signature: PartialSignature) -> &PartialSignature {
        self.peers_partial_sig.get_or_insert(partial_signature)
    }

    pub fn set_adaptor_point(&mut self, adaptor_point: Point) -> Result<&Point> {
        // In order to have a better chance of provable security, don't allow the adaptor point to
        // be set after our local nonce share has already been initialized, as otherwise an attacker
        // may gain too much control over the challenge hash or final adapted signature nonce:
        if self.my_nonce_pair_share.is_none() && self.aggregated_sig.is_none() {
            self.adaptor_point = adaptor_point.into();
        }
        match self.adaptor_point {
            MaybePoint::Valid(ref x) if *x == adaptor_point => Ok(x),
            _ => Err(MultisigErrorKind::MismatchedSigs)
        }
    }

    pub fn aggregated_sig(&self) -> Result<&AdaptorSignature> {
        self.aggregated_sig.as_ref().ok_or(MultisigErrorKind::MissingAggSig)
    }

    pub fn init_my_nonce_share(&mut self) -> Result<()> {
        let seckey = self.tweaked_key_ctx()?.my_prv_key;
        let aggregated_pubkey = self.tweaked_key_ctx()?.key_agg_ctx.aggregated_pubkey();
        // TODO: Consider making the RNG configurable, to aid unit testing:
        self.my_nonce_pair_share.get_or_insert_with(||
            NoncePair::new(&mut rand::rng(), seckey, aggregated_pubkey));
        Ok(())
    }

    pub fn aggregate_nonce_shares(&mut self) -> Result<&AggNonce> {
        let agg_nonce = AggNonce::sum([self.my_nonce_share()?, self.peers_nonce_share()?]);
        Ok(self.aggregated_nonce.insert(agg_nonce))
    }

    pub fn sign_partial(&mut self, message: TapSighash) -> Result<&PartialSignature> {
        if self.aggregated_sig.is_some() && self.my_partial_sig.is_none() {
            return Err(MultisigErrorKind::MismatchedSigs);
        }
        if self.message == Some(message) {
            return self.my_partial_sig();
        }
        let tweaked_key_ctx = self.tweaked_key_ctx.as_ref()
            .ok_or(MultisigErrorKind::MissingAggPubKey)?;
        let key_agg_ctx = &tweaked_key_ctx.key_agg_ctx;
        let seckey = tweaked_key_ctx.my_prv_key;
        let aggregated_nonce = self.aggregated_nonce.as_ref()
            .ok_or(MultisigErrorKind::MissingAggNonce)?;
        let secnonce = self.my_nonce_pair_share.as_mut()
            .ok_or(MultisigErrorKind::MissingNonceShare)?.sec_nonce.take()
            .ok_or(MultisigErrorKind::NonceReuse)?;

        let sig = musig2::adaptor::sign_partial(key_agg_ctx, seckey, secnonce, aggregated_nonce,
            self.adaptor_point, message.as_byte_array())?;
        self.message = Some(message);
        Ok(self.my_partial_sig.insert(sig))
    }

    pub fn aggregate_partial_signatures(&mut self) -> Result<&AdaptorSignature> {
        let key_agg_ctx = &self.tweaked_key_ctx()?.key_agg_ctx;
        let aggregated_nonce = &self.aggregated_nonce.as_ref()
            .ok_or(MultisigErrorKind::MissingAggNonce)?;
        let partial_signatures = [*self.my_partial_sig()?, *self.peers_partial_sig()?];
        let message = self.message.as_ref()
            .ok_or(MultisigErrorKind::MissingPartialSig)?;

        let sig = musig2::adaptor::aggregate_partial_signatures(key_agg_ctx, aggregated_nonce,
            self.adaptor_point, partial_signatures, message)?;
        Ok(self.aggregated_sig.insert(sig))
    }

    pub fn compute_taproot_signature(&self, adaptor_secret: MaybeScalar) -> Result<Signature> {
        let adaptor_sig = self.aggregated_sig
            .ok_or(MultisigErrorKind::MissingAggSig)?;
        if self.adaptor_point != adaptor_secret.base_point_mul() {
            return Err(MultisigErrorKind::MismatchedKeyPair);
        }
        let sig_bytes: [u8; 64] = adaptor_sig.adapt(adaptor_secret)
            .ok_or(MultisigErrorKind::ZeroNonce)?;
        Ok(Signature::from_slice(&sig_bytes).expect("len = 64"))
    }

    pub fn reveal_adaptor_secret(&self, signature: Signature) -> Result<Scalar> {
        let final_sig = LiftedSignature::from_bytes(&signature.serialize())?;
        let adaptor_sig = self.aggregated_sig()?;
        let adaptor_secret: MaybeScalar = adaptor_sig.reveal_secret(&final_sig)
            .ok_or(MultisigErrorKind::MismatchedSigs)?;
        Ok(adaptor_secret.try_into()?)
    }

    pub fn sign_solo(&mut self, message: TapSighash, peers_prv_key: Scalar) -> Result<&AdaptorSignature> {
        let tweaked_key_ctx = self.tweaked_key_ctx()?;
        let mut prv_key_shares = [tweaked_key_ctx.my_prv_key, peers_prv_key];
        prv_key_shares.sort_by_key(Scalar::base_point_mul);
        let seckey: Scalar = tweaked_key_ctx.key_agg_ctx.aggregated_seckey(prv_key_shares)?;

        let sig = musig2::deterministic::adaptor::sign_solo(seckey, message, self.adaptor_point);
        if self.aggregated_sig.is_some_and(|s| s != sig) {
            return Err(MultisigErrorKind::MismatchedSigs);
        }
        self.message = Some(message);
        Ok(self.aggregated_sig.insert(sig))
    }
}

pub trait PointExt {
    fn to_public_key(&self) -> PublicKey;
}

impl PointExt for Point {
    fn to_public_key(&self) -> PublicKey {
        // NOTE: We have to round-trip the public key because 'musig2' & 'bitcoin' currently use
        // different versions of the 'secp256k1' crate. TODO: Consider unifying the versions.
        PublicKey::from_slice(&self.serialize_uncompressed())
            .expect("curve point should have a valid uncompressed DER encoding")
    }
}

type Result<T, E = MultisigErrorKind> = std::result::Result<T, E>;

#[derive(Error, Debug)]
#[error(transparent)]
#[non_exhaustive]
pub enum MultisigErrorKind {
    #[error("missing private key")]
    MissingPrvKey,
    #[error("missing key share")]
    MissingKeyShare,
    #[error("missing nonce share")]
    MissingNonceShare,
    #[error("missing partial signature")]
    MissingPartialSig,
    #[error("missing aggregated pubkey")]
    MissingAggPubKey,
    #[error("missing aggregated signature")]
    MissingAggSig,
    #[error("missing aggregated nonce")]
    MissingAggNonce,
    #[error("nonce has already been used")]
    NonceReuse,
    #[error("nonce is zero")]
    ZeroNonce,
    #[error("public-private key mismatch")]
    MismatchedKeyPair,
    #[error("mismatched adaptor and signature")]
    MismatchedSigs,
    KeyAgg(#[from] musig2::errors::KeyAggError),
    Signing(#[from] musig2::errors::SigningError),
    Verify(#[from] musig2::errors::VerifyError),
    Tweak(#[from] musig2::errors::TweakError),
    InvalidSecretKeys(#[from] musig2::errors::InvalidSecretKeysError),
    DecodeLiftedSignature(#[from] musig2::errors::DecodeError<LiftedSignature>),
    ZeroScalar(#[from] musig2::secp::errors::ZeroScalarError),
}
