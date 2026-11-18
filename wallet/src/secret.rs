//! Types that hold secrets in memory: key shares, keys of payouts and secret nonces.
//!
//! A secret that is kept in one of these types is on the heap, in one place, and this place is
//! overwritten with zeros when the value is dropped. The types cannot be copied without a
//! visible `clone`, and their debug output never shows the secret.
//!
//! The limit of this protection: a computation needs the secret in the form of the cryptographic
//! libraries, which is a plain value that is copied freely and never wiped. The methods named
//! `expose` return this form. A caller uses the result for one computation and does not keep it:
//! what must be kept is kept as one of the types of this module.

use std::fmt;

use secp::{Point, Scalar};
use thiserror::Error;
use zeroize::{Zeroize as _, ZeroizeOnDrop, Zeroizing};

/// Secret bytes. They are wiped when the value is dropped.
pub struct SecretBytes(Box<[u8]>);

impl SecretBytes {
    /// Copies `bytes` into a new secret. The caller is responsible for its own copy.
    pub fn new(bytes: &[u8]) -> Self {
        Self(bytes.into())
    }

    /// The secret bytes, for one computation or one conversion.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Overwrites the bytes with zeros, in a way that the compiler does not remove.
    fn wipe(&mut self) {
        self.0.zeroize();
    }
}

impl Clone for SecretBytes {
    fn clone(&self) -> Self {
        Self::new(&self.0)
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl ZeroizeOnDrop for SecretBytes {}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBytes(<redacted>)")
    }
}

impl PartialEq for SecretBytes {
    /// Compares in a time that does not depend on the position of the first difference.
    fn eq(&self, other: &Self) -> bool {
        let difference = self
            .0
            .iter()
            .zip(other.0.iter())
            .fold(0, |difference, (a, b)| difference | (a ^ b));
        self.0.len() == other.0.len() && difference == 0
    }
}

impl Eq for SecretBytes {}

/// The bytes are not a secret scalar: they are no number between 1 and the order of the curve
/// less 1, in 32 bytes.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("not a valid secret scalar of the curve")]
pub struct InvalidSecretScalar;

/// A secret scalar of the curve: a secret key share of a trade, the secret key of a payout, or
/// another secret key. It is wiped when the value is dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretScalar(SecretBytes);

impl SecretScalar {
    /// Takes over a scalar that a computation returned. The value that the caller holds is a
    /// plain copy, which the caller must not keep.
    pub fn new(scalar: Scalar) -> Self {
        let bytes = Zeroizing::new(scalar.serialize());
        Self(SecretBytes::new(&bytes[..]))
    }

    /// Reads a secret scalar from its 32 bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, InvalidSecretScalar> {
        Scalar::from_slice(bytes).map_err(|_| InvalidSecretScalar)?;
        Ok(Self(SecretBytes::new(bytes)))
    }

    /// The scalar in the form of the cryptographic library, for one computation.
    pub fn expose(&self) -> Scalar {
        Scalar::from_slice(self.0.expose()).expect("the bytes were checked when they were stored")
    }

    /// The 32 bytes of the scalar, for a message or for the storage. They are wiped when the
    /// result is dropped.
    pub fn to_bytes(&self) -> Zeroizing<[u8; 32]> {
        let mut bytes = Zeroizing::new([0; 32]);
        bytes.copy_from_slice(self.0.expose());
        bytes
    }

    /// The public key of the secret.
    pub fn public_point(&self) -> Point {
        self.expose().base_point_mul()
    }
}

impl From<Scalar> for SecretScalar {
    fn from(scalar: Scalar) -> Self {
        Self::new(scalar)
    }
}

impl ZeroizeOnDrop for SecretScalar {}

impl fmt::Debug for SecretScalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretScalar(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(byte: u8) -> Scalar {
        Scalar::from_slice(&[byte; 32]).expect("valid scalar")
    }

    #[test]
    fn secret_scalar_keeps_its_value_and_hides_it() {
        let secret = SecretScalar::new(scalar(7));
        assert_eq!(secret.expose(), scalar(7));
        assert_eq!(*secret.to_bytes(), [7; 32]);
        assert_eq!(secret.public_point(), scalar(7).base_point_mul());
        assert_eq!(secret.clone(), secret);
        assert_ne!(SecretScalar::new(scalar(8)), secret);
        assert_eq!(
            SecretScalar::from_slice(&[7; 32]).expect("valid scalar"),
            secret
        );
        let shown = format!("{secret:?} {:?}", SecretBytes::new(&[7; 4]));
        assert!(!shown.contains('7'), "{shown}");
    }

    #[test]
    fn invalid_bytes_are_no_secret_scalar() {
        for invalid in [&[0; 32][..], &[0xff; 32], &[1; 31], &[1; 33], &[]] {
            assert_eq!(
                SecretScalar::from_slice(invalid),
                Err(InvalidSecretScalar),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn secret_bytes_compare_by_content_and_length() {
        assert_eq!(SecretBytes::new(&[1, 2, 3]), SecretBytes::new(&[1, 2, 3]));
        assert_ne!(SecretBytes::new(&[1, 2, 3]), SecretBytes::new(&[1, 2, 4]));
        assert_ne!(SecretBytes::new(&[1, 2, 3]), SecretBytes::new(&[1, 2]));
    }

    /// The wipe that `drop` performs overwrites the bytes where they are.
    #[test]
    fn wipe_overwrites_the_bytes_in_place() {
        let mut secret = SecretBytes::new(&[0xab; 32]);
        let location = secret.0.as_ptr();
        secret.wipe();
        assert_eq!(secret.expose(), &[0; 32]);
        assert_eq!(secret.0.as_ptr(), location);
    }
}
