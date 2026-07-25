//! PKCE S256 (RFC 7636).
//!
//! Load-bearing invariant (checked against Pi `pkce.ts:29`): the `challenge`
//! hashes the **UTF-8 bytes of the `verifier` STRING** (already base64url), NOT the
//! 32 raw random bytes. `Sha256::digest(verifier.as_bytes())`, never
//! `Sha256::digest(&random_bytes)`: the two produce different
//! challenges and the server would reject the second one.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &"Secret(***)")
            .field("challenge", &"Secret(***)")
            .finish()
    }
}

impl Pkce {
    /// Generates a fresh verifier/challenge pair (32 bytes of entropy).
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = Self::challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }

    /// `challenge = base64url_nopad(SHA-256(verifier.as_bytes()))`.
    pub fn challenge_for(verifier: &str) -> String {
        let digest = Sha256::digest(verifier.as_bytes());
        URL_SAFE_NO_PAD.encode(digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference vector of RFC 7636, Appendix B.
    #[test]
    fn rfc7636_known_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(Pkce::challenge_for(verifier), expected);
    }

    #[test]
    fn generated_pkce_has_expected_shape() {
        let p = Pkce::generate();
        // 32 bytes in base64url without padding = 43 characters.
        assert_eq!(p.verifier.len(), 43);
        assert_eq!(p.challenge.len(), 43);
        // the challenge does derive from the generated verifier
        assert_eq!(p.challenge, Pkce::challenge_for(&p.verifier));
        assert!(
            !p.verifier.contains('=') && !p.verifier.contains('+') && !p.verifier.contains('/')
        );
    }

    #[test]
    fn debug_redacts_verifier_and_challenge() {
        let p = Pkce {
            verifier: "VERIFIER_SECRET".into(),
            challenge: "CHALLENGE_PUBLIC_BUT_LINKABLE".into(),
        };
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("VERIFIER_SECRET"));
        assert!(!dbg.contains("CHALLENGE_PUBLIC_BUT_LINKABLE"));
        assert!(dbg.contains("Secret(***)"));
    }
}
