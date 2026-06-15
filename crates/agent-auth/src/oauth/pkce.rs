//! PKCE S256 (RFC 7636).
//!
//! ⚠️ Invariant load-bearing (vérifié contre Pi `pkce.ts:29`) : le `challenge`
//! hashe les **octets UTF-8 de la STRING `verifier`** (déjà base64url), PAS les
//! 32 octets aléatoires bruts. `Sha256::digest(verifier.as_bytes())`, jamais
//! `Sha256::digest(&random_bytes)` — les deux produisent des challenges
//! différents et le serveur rejetterait le second.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// Génère un couple verifier/challenge frais (32 octets d'entropie).
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

    /// Vecteur de référence RFC 7636, Appendix B.
    #[test]
    fn rfc7636_known_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(Pkce::challenge_for(verifier), expected);
    }

    #[test]
    fn generated_pkce_has_expected_shape() {
        let p = Pkce::generate();
        // 32 octets en base64url sans padding = 43 caractères.
        assert_eq!(p.verifier.len(), 43);
        assert_eq!(p.challenge.len(), 43);
        // le challenge dérive bien du verifier généré
        assert_eq!(p.challenge, Pkce::challenge_for(&p.verifier));
        assert!(
            !p.verifier.contains('=') && !p.verifier.contains('+') && !p.verifier.contains('/')
        );
    }
}
