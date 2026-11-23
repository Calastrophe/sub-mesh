//! PKCE (RFC 7636, S256) and random OAuth state values.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use sha2::{Digest, Sha256};

/// A PKCE code verifier and its S256 challenge.
pub(crate) struct Pkce {
    pub(crate) verifier: String,
    pub(crate) challenge: String,
}

impl Pkce {
    /// A fresh verifier of 43 URL-safe characters (256 bits of entropy).
    pub(crate) fn generate() -> Self {
        Self::from_verifier(random_token())
    }

    fn from_verifier(verifier: String) -> Self {
        let digest = Sha256::digest(verifier.as_bytes());
        Pkce {
            challenge: URL_SAFE_NO_PAD.encode(digest),
            verifier,
        }
    }
}

/// 32 random bytes as 43 URL-safe base64 characters. Used for the PKCE
/// verifier and the OAuth `state`.
pub(crate) fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_rfc_7636_test_vector() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_owned());
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_values_are_valid_and_distinct() {
        let a = Pkce::generate();
        let b = Pkce::generate();
        assert_eq!(a.verifier.len(), 43);
        assert!(
            a.verifier
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(random_token(), random_token());
    }
}
