//! Symmetric crypto primitives exposed to the Lua sandbox.
//!
//! These let flows decrypt secrets (e.g. API credentials) that are stored
//! encrypted at rest, without the caller ever passing plaintext keys through
//! the run input. The on-disk format mirrors the Web Crypto API:
//!
//! - Cipher: **AES-256-GCM**, 12-byte IV, 16-byte (128-bit) auth tag appended
//!   to the ciphertext.
//! - Key derivation: **PBKDF2-HMAC-SHA256**, 16-byte salt, 32-byte derived key.
//! - Serialized blob = `base64( salt[16] || iv[12] || ciphertext || tag[16] )`.
//!
//! Implemented on top of `ring`, which provides constant-time tag verification
//! and a self-contained crypto stack (no RustCrypto trait-version juggling).
//!
//! Security notes:
//! - Plaintext and passphrases are never logged.
//! - GCM auth-tag failure (wrong passphrase / tampered ciphertext) returns a
//!   clean Lua error with no partial plaintext and no panic.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mlua::{Lua, Result as LuaResult};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::pbkdf2;
use std::num::NonZeroU32;

/// Default PBKDF2 iteration count, matching the consumer's Web Crypto config.
const DEFAULT_PBKDF2_ITERATIONS: u32 = 600_000;

/// Layout constants for the serialized blob.
const SALT_LEN: usize = 16;
const AES_KEY_LEN: usize = 32;
const GCM_TAG_LEN: usize = 16;

/// Derive a key via PBKDF2-HMAC-SHA256.
fn derive_pbkdf2_sha256(
    passphrase: &[u8],
    salt: &[u8],
    iterations: u32,
    key_len: usize,
) -> Result<Vec<u8>, String> {
    let iters = NonZeroU32::new(iterations).ok_or("iterations must be greater than 0")?;
    if key_len == 0 {
        return Err("key length must be greater than 0".to_string());
    }
    let mut out = vec![0u8; key_len];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iters,
        salt,
        passphrase,
        &mut out,
    );
    Ok(out)
}

/// AES-256-GCM decrypt. `ciphertext_with_tag` is the ciphertext with the
/// 16-byte auth tag appended (Web Crypto / ring layout). Returns the plaintext
/// or an error on auth-tag failure — no partial plaintext is ever returned.
fn aes_256_gcm_decrypt(
    key: &[u8],
    iv: &[u8],
    ciphertext_with_tag: &[u8],
) -> Result<Vec<u8>, String> {
    if key.len() != AES_KEY_LEN {
        return Err(format!(
            "AES-256-GCM key must be {} bytes, got {}",
            AES_KEY_LEN,
            key.len()
        ));
    }
    if iv.len() != NONCE_LEN {
        return Err(format!(
            "AES-256-GCM IV must be {} bytes, got {}",
            NONCE_LEN,
            iv.len()
        ));
    }
    if ciphertext_with_tag.len() < GCM_TAG_LEN {
        return Err("ciphertext is shorter than the GCM auth tag".to_string());
    }

    let unbound = UnboundKey::new(&AES_256_GCM, key).map_err(|_| "invalid AES key".to_string())?;
    let sealing = LessSafeKey::new(unbound);
    // `iv.len() == NONCE_LEN` checked above, so this cannot fail.
    let nonce = Nonce::try_assume_unique_for_key(iv).map_err(|_| "invalid IV".to_string())?;

    let mut in_out = ciphertext_with_tag.to_vec();
    let plaintext = sealing
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| "decryption failed: authentication tag mismatch".to_string())?;
    Ok(plaintext.to_vec())
}

/// Decrypt a `base64( salt[16] || iv[12] || ciphertext || tag[16] )` blob:
/// derive the key via PBKDF2-HMAC-SHA256(passphrase, salt, iterations, 32),
/// then AES-256-GCM-decrypt.
fn decrypt_blob_pbkdf2(
    blob_b64: &str,
    passphrase: &[u8],
    iterations: u32,
) -> Result<Vec<u8>, String> {
    let blob = STANDARD
        .decode(blob_b64.trim())
        .map_err(|e| format!("invalid base64 blob: {}", e))?;

    let min_len = SALT_LEN + NONCE_LEN + GCM_TAG_LEN;
    if blob.len() < min_len {
        return Err(format!(
            "blob too short: need at least {} bytes (salt+iv+tag), got {}",
            min_len,
            blob.len()
        ));
    }

    let salt = &blob[0..SALT_LEN];
    let iv = &blob[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ciphertext_with_tag = &blob[SALT_LEN + NONCE_LEN..];

    let key = derive_pbkdf2_sha256(passphrase, salt, iterations, AES_KEY_LEN)?;
    aes_256_gcm_decrypt(&key, iv, ciphertext_with_tag)
}

/// Register crypto globals on the given Lua VM. Byte data is passed as Lua
/// strings (raw bytes), consistent with how the rest of the sandbox treats
/// binary data.
pub fn register_crypto_globals(lua: &Lua) -> LuaResult<()> {
    // pbkdf2_sha256(passphrase, salt, iterations, key_len) -> raw key bytes
    let pbkdf2_fn = lua.create_function(
        |lua, (passphrase, salt, iterations, key_len): (mlua::String, mlua::String, u32, usize)| {
            let key = derive_pbkdf2_sha256(
                &passphrase.as_bytes(),
                &salt.as_bytes(),
                iterations,
                key_len,
            )
            .map_err(mlua::Error::external)?;
            lua.create_string(&key)
        },
    )?;
    lua.globals().set("pbkdf2_sha256", pbkdf2_fn)?;

    // aes_256_gcm_decrypt(key, iv, ciphertext_with_tag) -> plaintext bytes
    let aes_decrypt_fn = lua.create_function(
        |lua, (key, iv, ciphertext): (mlua::String, mlua::String, mlua::String)| {
            let plaintext =
                aes_256_gcm_decrypt(&key.as_bytes(), &iv.as_bytes(), &ciphertext.as_bytes())
                    .map_err(mlua::Error::external)?;
            lua.create_string(&plaintext)
        },
    )?;
    lua.globals().set("aes_256_gcm_decrypt", aes_decrypt_fn)?;

    // aes_gcm_decrypt_pbkdf2(blob_b64, passphrase, iterations?) -> plaintext bytes
    let convenience_fn = lua.create_function(
        |lua, (blob_b64, passphrase, iterations): (String, mlua::String, Option<u32>)| {
            let iterations = iterations.unwrap_or(DEFAULT_PBKDF2_ITERATIONS);
            let plaintext = decrypt_blob_pbkdf2(&blob_b64, &passphrase.as_bytes(), iterations)
                .map_err(mlua::Error::external)?;
            lua.create_string(&plaintext)
        },
    )?;
    lua.globals()
        .set("aes_gcm_decrypt_pbkdf2", convenience_fn)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::aead::{AES_256_GCM, Aad, BoundKey, Nonce, NonceSequence, SealingKey, UnboundKey};
    use ring::error::Unspecified;

    /// One-shot nonce sequence for building known-vector ciphertexts in tests.
    struct OneNonce(Option<[u8; NONCE_LEN]>);
    impl NonceSequence for OneNonce {
        fn advance(&mut self) -> Result<Nonce, Unspecified> {
            let n = self.0.take().ok_or(Unspecified)?;
            Ok(Nonce::assume_unique_for_key(n))
        }
    }

    /// Encrypt helper that mirrors the consumer's Web Crypto layout, used only
    /// to generate test vectors (encryption is out of scope for the runtime).
    fn encrypt_blob(plaintext: &[u8], passphrase: &[u8], iterations: u32) -> String {
        let salt = [7u8; SALT_LEN];
        let iv = [9u8; NONCE_LEN];
        let key = derive_pbkdf2_sha256(passphrase, &salt, iterations, AES_KEY_LEN).unwrap();

        let unbound = UnboundKey::new(&AES_256_GCM, &key).unwrap();
        let mut sealing = SealingKey::new(unbound, OneNonce(Some(iv)));
        let mut buf = plaintext.to_vec();
        sealing
            .seal_in_place_append_tag(Aad::empty(), &mut buf)
            .unwrap();

        let mut blob = Vec::new();
        blob.extend_from_slice(&salt);
        blob.extend_from_slice(&iv);
        blob.extend_from_slice(&buf);
        STANDARD.encode(&blob)
    }

    #[test]
    fn round_trip_pbkdf2_aes_gcm() {
        let passphrase = b"correct horse battery staple";
        let plaintext = b"sk-secret-api-key-1234567890";
        let blob = encrypt_blob(plaintext, passphrase, 600_000);

        let got = decrypt_blob_pbkdf2(&blob, passphrase, 600_000).unwrap();
        assert_eq!(got, plaintext);
    }

    #[test]
    fn wrong_passphrase_fails_cleanly() {
        let blob = encrypt_blob(b"hello", b"right-pass", 600_000);
        let err = decrypt_blob_pbkdf2(&blob, b"wrong-pass", 600_000).unwrap_err();
        assert!(err.contains("authentication tag mismatch"), "got: {}", err);
    }

    #[test]
    fn tampered_ciphertext_fails_cleanly() {
        let blob = encrypt_blob(b"hello world", b"pass", 600_000);
        let mut raw = STANDARD.decode(&blob).unwrap();
        // Flip a bit inside the ciphertext region (after salt+iv).
        let idx = SALT_LEN + NONCE_LEN + 1;
        raw[idx] ^= 0x01;
        let tampered = STANDARD.encode(&raw);
        assert!(decrypt_blob_pbkdf2(&tampered, b"pass", 600_000).is_err());
    }

    #[test]
    fn blob_too_short_errors() {
        let tiny = STANDARD.encode([0u8; 10]);
        let err = decrypt_blob_pbkdf2(&tiny, b"pass", 600_000).unwrap_err();
        assert!(err.contains("too short"), "got: {}", err);
    }

    #[test]
    fn default_iterations_match_web_crypto() {
        // A blob produced at the default 600k iters must decrypt when the
        // caller omits the iterations argument (defaulted in the Lua wrapper).
        let blob = encrypt_blob(b"data", b"pass", DEFAULT_PBKDF2_ITERATIONS);
        let got = decrypt_blob_pbkdf2(&blob, b"pass", DEFAULT_PBKDF2_ITERATIONS).unwrap();
        assert_eq!(got, b"data");
    }

    #[test]
    fn low_level_primitives_compose() {
        // pbkdf2_sha256 + aes_256_gcm_decrypt should reproduce the convenience path.
        let passphrase = b"pp";
        let blob = encrypt_blob(b"compose-me", passphrase, 1000);
        let raw = STANDARD.decode(&blob).unwrap();
        let salt = &raw[0..SALT_LEN];
        let iv = &raw[SALT_LEN..SALT_LEN + NONCE_LEN];
        let ct = &raw[SALT_LEN + NONCE_LEN..];

        let key = derive_pbkdf2_sha256(passphrase, salt, 1000, AES_KEY_LEN).unwrap();
        let pt = aes_256_gcm_decrypt(&key, iv, ct).unwrap();
        assert_eq!(pt, b"compose-me");
    }

    #[test]
    fn aes_decrypt_rejects_bad_key_len() {
        let err = aes_256_gcm_decrypt(&[0u8; 16], &[0u8; 12], &[0u8; 32]).unwrap_err();
        assert!(err.contains("key must be 32 bytes"), "got: {}", err);
    }
}
