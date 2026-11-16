//! Team-code key derivation and end-to-end encryption.
//!
//! A *team code* is a random 120-bit secret shown to humans as
//! `XXXX-XXXX-XXXX-XXXX-XXXX-XXXX` (RFC 4648 base32). Every member derives the
//! same keys from it:
//!
//! * `auth_token` – presented to the relay; the relay hashes it to find the team room.
//! * `team_id`    – `sha256("poke-team-id/v1:" + auth_token)[..16]` as hex. The relay
//!   computes the same value, so it never needs any per-team configuration.
//! * `enc_key`    – XChaCha20-Poly1305 key. Never leaves the client, so the relay
//!   only ever sees ciphertext.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, Generate, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

use crate::Error;

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const CODE_BYTES: usize = 15; // 120 bits -> exactly 24 base32 chars
const CODE_CHARS: usize = 24;
const HKDF_SALT: &[u8] = b"poke-the-colleague/v1";
const NONCE_LEN: usize = 24;

/// Generate a fresh random team code, e.g. `K7QX-M2PA-...`.
pub fn generate_team_code() -> String {
    let mut bytes = [0u8; CODE_BYTES];
    rand::fill(&mut bytes);
    group(&base32_encode(&bytes))
}

/// Normalise user input (case, spaces, dashes) and validate it.
pub fn normalize_team_code(input: &str) -> Result<String, Error> {
    let cleaned: String = input
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\t'))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if cleaned.len() != CODE_CHARS || !cleaned.bytes().all(|b| BASE32.contains(&b)) {
        return Err(Error::InvalidTeamCode);
    }
    Ok(group(&cleaned))
}

fn group(s: &str) -> String {
    s.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join("-")
}

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Keys derived from a team code.
#[derive(Clone)]
pub struct TeamKeys {
    auth_token: String,
    team_id: String,
    cipher: XChaCha20Poly1305,
}

impl std::fmt::Debug for TeamKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamKeys").field("team_id", &self.team_id).finish_non_exhaustive()
    }
}

impl TeamKeys {
    pub fn derive(team_code: &str) -> Result<Self, Error> {
        let code = normalize_team_code(team_code)?;
        let ikm: String = code.chars().filter(|c| *c != '-').collect();
        let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), ikm.as_bytes());

        let mut auth = [0u8; 32];
        let mut enc = [0u8; 32];
        hk.expand(b"relay-auth", &mut auth).expect("valid length");
        hk.expand(b"e2e-encryption", &mut enc).expect("valid length");

        let auth_token = B64.encode(auth);
        let team_id = team_id_for_token(&auth_token);
        let cipher = XChaCha20Poly1305::new_from_slice(&enc).expect("32-byte key");
        Ok(Self { auth_token, team_id, cipher })
    }

    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    /// Public, non-secret identifier of the team (used for relay allow-lists).
    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    /// Encrypt `plaintext`, binding it to `aad` (context such as sender/recipient).
    /// Output: base64url(nonce || ciphertext || tag).
    pub fn seal(&self, aad: &str, plaintext: &[u8]) -> String {
        let nonce = XNonce::generate();
        let ct = self
            .cipher
            .encrypt(&nonce, Payload { msg: plaintext, aad: aad.as_bytes() })
            .expect("encryption cannot fail for in-memory buffers");
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&ct);
        B64.encode(out)
    }

    pub fn open(&self, aad: &str, sealed: &str) -> Result<Vec<u8>, Error> {
        let raw = B64.decode(sealed).map_err(|_| Error::Decrypt)?;
        if raw.len() < NONCE_LEN + 16 {
            return Err(Error::Decrypt);
        }
        let (nonce, ct) = raw.split_at(NONCE_LEN);
        let nonce = XNonce::try_from(nonce).map_err(|_| Error::Decrypt)?;
        self.cipher
            .decrypt(&nonce, Payload { msg: ct, aad: aad.as_bytes() })
            .map_err(|_| Error::Decrypt)
    }
}

/// Must match `teamIdForToken` in `relay/src/index.ts`.
pub fn team_id_for_token(auth_token: &str) -> String {
    let digest = Sha256::digest(format!("poke-team-id/v1:{auth_token}").as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_codes_are_valid_and_unique() {
        let a = generate_team_code();
        let b = generate_team_code();
        assert_ne!(a, b);
        assert_eq!(a.len(), 29); // 24 chars + 5 dashes
        assert_eq!(normalize_team_code(&a).unwrap(), a);
    }

    #[test]
    fn normalization_is_forgiving() {
        let code = generate_team_code();
        let messy = format!("  {} ", code.to_lowercase().replace('-', " "));
        assert_eq!(normalize_team_code(&messy).unwrap(), code);
        assert!(normalize_team_code("too-short").is_err());
        assert!(normalize_team_code("0000-1111-0000-1111-0000-1111").is_err()); // 0/1 not base32
    }

    #[test]
    fn same_code_same_keys() {
        let code = generate_team_code();
        let a = TeamKeys::derive(&code).unwrap();
        let b = TeamKeys::derive(&code.to_lowercase()).unwrap();
        assert_eq!(a.auth_token(), b.auth_token());
        assert_eq!(a.team_id(), b.team_id());
        assert_eq!(a.team_id().len(), 32);
        let sealed = a.seal("ctx", b"hello");
        assert_eq!(b.open("ctx", &sealed).unwrap(), b"hello");
    }

    #[test]
    fn wrong_key_or_aad_fails() {
        let a = TeamKeys::derive(&generate_team_code()).unwrap();
        let b = TeamKeys::derive(&generate_team_code()).unwrap();
        let sealed = a.seal("poke|x|y|1", b"secret");
        assert!(b.open("poke|x|y|1", &sealed).is_err());
        assert!(a.open("poke|x|z|1", &sealed).is_err());
        assert!(a.open("poke|x|y|1", "garbage").is_err());
    }

    #[test]
    fn known_team_id_vector() {
        // Shared with the relay test so both sides hash identically.
        assert_eq!(team_id_for_token("abc"), "0501137013ef1fc56fd042e5ac6cb1a6");
    }
}
