//! The GitHub App's JSON Web Token (RS256), signed with the app's private key.
//!
//! GitHub issues the key as PKCS#1 PEM (`BEGIN RSA PRIVATE KEY`); PKCS#8 works too. The token
//! lives at most ten minutes; Schlange backdates `iat` by 60 seconds against clock drift and
//! sets `exp` nine minutes ahead, as GitHub recommends.

use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject;

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("the private key is not PEM: {0}")]
    Pem(String),
    #[error("the private key is not an RSA key ring accepts: {0}")]
    Rsa(String),
    #[error("only RSA keys are supported (GitHub App keys are RSA)")]
    NotRsa,
}

/// URL-safe base64 without padding (RFC 4648, section 5), as JWT uses.
pub fn base64url(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// Decodes standard or URL-safe base64, ignoring whitespace and padding.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The app's signing key.
pub struct AppKey {
    key: RsaKeyPair,
    rng: SystemRandom,
}

impl std::fmt::Debug for AppKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppKey(..)")
    }
}

impl AppKey {
    pub fn from_pem(pem: &[u8]) -> Result<Self, KeyError> {
        let der = PrivateKeyDer::from_pem_slice(pem).map_err(|e| KeyError::Pem(e.to_string()))?;
        let key = match &der {
            PrivateKeyDer::Pkcs1(k) => RsaKeyPair::from_der(k.secret_pkcs1_der()),
            PrivateKeyDer::Pkcs8(k) => RsaKeyPair::from_pkcs8(k.secret_pkcs8_der()),
            _ => return Err(KeyError::NotRsa),
        }
        .map_err(|e| KeyError::Rsa(e.to_string()))?;
        Ok(AppKey {
            key,
            rng: SystemRandom::new(),
        })
    }

    /// The public key as DER `RSAPublicKey` (the fake GitHub verifies tokens with it).
    pub fn public_key_der(&self) -> Vec<u8> {
        use ring::signature::KeyPair;
        self.key.public_key().as_ref().to_vec()
    }

    /// A JWT for app `app_id` (numeric id or client id), valid from `now - 60` to `now + 540`.
    pub fn jwt(&self, app_id: &str, now: u64) -> Result<String, KeyError> {
        let header = base64url(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = serde_json::json!({
            "iat": now.saturating_sub(60),
            "exp": now + 540,
            "iss": app_id,
        });
        let payload = base64url(claims.to_string().as_bytes());
        let signing_input = format!("{header}.{payload}");
        let mut sig = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &self.rng,
                signing_input.as_bytes(),
                &mut sig,
            )
            .map_err(|e| KeyError::Rsa(e.to_string()))?;
        Ok(format!("{signing_input}.{}", base64url(&sig)))
    }
}

/// Verifies an RS256 JWT against a DER `RSAPublicKey` and returns its claims (for the fake
/// GitHub and for tests; Schlange itself never verifies tokens).
pub fn verify(token: &str, public_key_der: &[u8]) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let (h, p, s) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let sig = base64_decode(s)?;
    ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        public_key_der,
    )
    .verify(format!("{h}.{p}").as_bytes(), &sig)
    .ok()?;
    serde_json::from_slice(&base64_decode(p)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = include_bytes!("../../schlange-github-fake/fixtures/test-app-key.pem");

    #[test]
    fn base64url_matches_rfc4648_vectors() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64_decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode("-_8").unwrap(), vec![0xfb, 0xff]);
        assert_eq!(base64_decode("Zm9v\nYmFy").unwrap(), b"foobar");
        assert!(base64_decode("Zm9v*").is_none());
    }

    #[test]
    fn jwt_is_rs256_with_backdated_iat_and_short_expiry() {
        let key = AppKey::from_pem(KEY).unwrap();
        let token = key.jwt("12345", 1_000_000).unwrap();
        let claims = verify(&token, &key.public_key_der()).expect("valid signature");
        assert_eq!(claims["iss"], "12345");
        assert_eq!(claims["iat"], 1_000_000 - 60);
        assert_eq!(claims["exp"], 1_000_000 + 540);
        let header: serde_json::Value =
            serde_json::from_slice(&base64_decode(token.split('.').next().unwrap()).unwrap())
                .unwrap();
        assert_eq!(header["alg"], "RS256");
        // A tampered payload fails.
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = base64url(br#"{"iss":"999"}"#);
        parts[1] = &forged;
        assert!(verify(&parts.join("."), &key.public_key_der()).is_none());
    }

    #[test]
    fn a_non_key_is_refused() {
        assert!(AppKey::from_pem(b"not a key").is_err());
    }
}
