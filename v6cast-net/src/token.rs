//! Group token: the pre-shared secret every peer of a group knows.

use std::fmt;

use subtle::ConstantTimeEq;
use v6cast_protocol::control::MAX_TOKEN_LEN;

use crate::error::{NetError, Result};

/// A group token of 1 to 255 bytes. `Debug` never prints the secret.
#[derive(Clone)]
pub struct GroupToken {
    /// Zero-padded copy, so comparisons always touch the same number of bytes.
    padded: [u8; MAX_TOKEN_LEN],
    len: u8,
}

impl GroupToken {
    pub fn new(token: impl AsRef<[u8]>) -> Result<Self> {
        let token = token.as_ref();
        if token.is_empty() {
            return Err(NetError::InvalidConfig("group token is empty"));
        }
        if token.len() > MAX_TOKEN_LEN {
            return Err(NetError::InvalidConfig(
                "group token is longer than 255 bytes",
            ));
        }
        let mut padded = [0u8; MAX_TOKEN_LEN];
        padded[..token.len()].copy_from_slice(token);
        Ok(Self {
            padded,
            len: token.len() as u8,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.padded[..usize::from(self.len)]
    }

    /// Compares `candidate` with the token in constant time.
    ///
    /// The time taken depends neither on where the bytes differ nor on the
    /// secret's length: both sides are compared as fixed 255-byte buffers and the
    /// lengths are compared separately. Empty or oversized candidates never match.
    pub fn verify(&self, candidate: &[u8]) -> bool {
        let mut other = [0u8; MAX_TOKEN_LEN];
        let fits = candidate.len() <= MAX_TOKEN_LEN;
        let n = candidate.len().min(MAX_TOKEN_LEN);
        other[..n].copy_from_slice(&candidate[..n]);
        let candidate_len = u16::try_from(candidate.len()).unwrap_or(u16::MAX);
        let same_len = u16::from(self.len).ct_eq(&candidate_len);
        let same_bytes = self.padded.ct_eq(&other);
        fits && bool::from(same_len & same_bytes)
    }
}

impl fmt::Debug for GroupToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupToken")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_only_the_exact_token() {
        let t = GroupToken::new("friends-only").unwrap();
        assert!(t.verify(b"friends-only"));
        for wrong in [
            &b""[..],
            b"friends-onl",
            b"friends-only!",
            b"Friends-only",
            b"friends-only\0",
        ] {
            assert!(!t.verify(wrong), "{wrong:?}");
        }
        assert!(!t.verify(&[b'x'; 300]));
    }

    #[test]
    fn trailing_zero_bytes_do_not_match_the_padding() {
        let t = GroupToken::new([1u8, 2, 3]).unwrap();
        assert!(!t.verify(&[1, 2, 3, 0]));
        assert!(!t.verify(&[1, 2, 3, 0, 0, 0]));
    }

    #[test]
    fn length_limits() {
        assert!(GroupToken::new("").is_err());
        assert!(GroupToken::new([7u8; MAX_TOKEN_LEN]).is_ok());
        assert!(GroupToken::new([7u8; MAX_TOKEN_LEN + 1]).is_err());
        let max = GroupToken::new([7u8; MAX_TOKEN_LEN]).unwrap();
        assert!(max.verify(&[7u8; MAX_TOKEN_LEN]));
    }

    #[test]
    fn debug_hides_the_secret() {
        let t = GroupToken::new("hunter2").unwrap();
        let shown = format!("{t:?}");
        assert!(!shown.contains("hunter2"));
        assert!(shown.contains("len: 7"));
    }
}
