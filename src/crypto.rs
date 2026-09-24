//! WinZIP AES-256 (AE-2), per APPNOTE.TXT 6.3.0 §7.2.
//!
//! Layout of an encrypted entry's *file data* field:
//!
//!     [ salt (16 bytes) | pw_verify (2 bytes) | ciphertext ... | HMAC-SHA1[..10] ]
//!
//! * key = PBKDF2-HMAC-SHA1(password, salt, 1000, 32 (enc) + 32 (mac) + 2 (verify))
//! * enc: AES-256-CTR, block counter starts at 1, little-endian in the low 4 bytes
//!   of a 16-byte counter block (rest zero). Compress-then-encrypt.
//! * mac: HMAC-SHA1 over the ciphertext, truncated to 10 bytes.
//! * salt length for AES-256 is 16 bytes, verify is 2 bytes.
//! * "Extra Field 0x9901" is emitted by the ZIP writer, not here.
//!
//! AE-2 (vs AE-1) stores CRC-32 = 0 in the local file header. We compute it
//! anyway for internal verification but zero it when serializing.

use aes::cipher::{KeyIvInit, StreamCipher};
use anyhow::{bail, Result};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;
type Aes256Ctr = ctr::Ctr128LE<aes::Aes256>;

pub const SALT_LEN: usize = 16;
pub const VERIFY_LEN: usize = 2;
pub const MAC_LEN: usize = 10;
pub const KEY_LEN: usize = 32; // AES-256
pub const MAC_KEY_LEN: usize = 32; // HMAC-SHA1 key size per WinZIP AES

/// Overhead bytes added to a plaintext body when it is AE-2 encrypted.
pub const AE2_OVERHEAD: usize = SALT_LEN + VERIFY_LEN + MAC_LEN;

/// Encrypted payload with all envelope bytes already assembled.
pub struct AesPayload {
    /// salt || pw_verify || ciphertext || mac10
    pub bytes: Vec<u8>,
}

/// Encrypt `plaintext` (already-compressed data) in place.
///
/// The input `buf` MUST have `SALT_LEN + VERIFY_LEN` bytes of prefix headroom
/// and `MAC_LEN` bytes of trailing headroom around the plaintext. The function
/// then:
///
/// 1. generates a random 16-byte salt into `buf[0..16]`
/// 2. derives PBKDF2 keys and writes the 2-byte password verifier into `buf[16..18]`
/// 3. encrypts the plaintext (currently at `buf[18..buf.len()-10]`) in place
/// 4. computes HMAC-SHA1 over the ciphertext and writes 10 bytes into the tail
///
/// This is one allocation (the input `buf` itself) instead of the previous
/// three (allocate plaintext copy, encrypt, then allocate output envelope).
pub fn encrypt_ae2_in_place(password: &str, buf: &mut Vec<u8>, plaintext_len: usize) -> Result<()> {
    if password.is_empty() {
        bail!("empty password");
    }
    let expected = SALT_LEN + VERIFY_LEN + plaintext_len + MAC_LEN;
    if buf.len() != expected {
        bail!(
            "buffer size mismatch: got {}, expected {} (salt+verify+plaintext+mac)",
            buf.len(),
            expected
        );
    }
    let mut salt = [0u8; SALT_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    encrypt_ae2_in_place_with_salt(password, &salt, buf, plaintext_len)
}

/// Deterministic variant used by tests. Same layout expectations as
/// `encrypt_ae2_in_place`.
pub fn encrypt_ae2_in_place_with_salt(
    password: &str,
    salt: &[u8; SALT_LEN],
    buf: &mut [u8],
    plaintext_len: usize,
) -> Result<()> {
    let expected = SALT_LEN + VERIFY_LEN + plaintext_len + MAC_LEN;
    if buf.len() != expected {
        bail!("buffer size mismatch");
    }

    let mut derived = [0u8; KEY_LEN + MAC_KEY_LEN + VERIFY_LEN];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), salt, 1000, &mut derived);

    let enc_key: [u8; KEY_LEN] = derived[..KEY_LEN].try_into().unwrap();
    let mac_key_bytes: [u8; MAC_KEY_LEN] =
        derived[KEY_LEN..KEY_LEN + MAC_KEY_LEN].try_into().unwrap();
    let verify: [u8; VERIFY_LEN] = derived[KEY_LEN + MAC_KEY_LEN..].try_into().unwrap();

    // Fill the salt + verifier prefix in place.
    buf[..SALT_LEN].copy_from_slice(salt);
    buf[SALT_LEN..SALT_LEN + VERIFY_LEN].copy_from_slice(&verify);

    // WinZIP AES: 128-bit LE counter starting at 1. We initialize the IV to
    // [1, 0, 0, ...] so the first plaintext block is encrypted with counter=1
    // directly — no wasted keystream block, no dummy-encrypt hack.
    let mut iv = [0u8; 16];
    iv[0] = 1;
    let mut cipher = Aes256Ctr::new((&enc_key).into(), (&iv).into());

    let pt_start = SALT_LEN + VERIFY_LEN;
    let pt_end = pt_start + plaintext_len;
    cipher.apply_keystream(&mut buf[pt_start..pt_end]);

    let mut mac = HmacSha1::new_from_slice(&mac_key_bytes).expect("hmac key size");
    mac.update(&buf[pt_start..pt_end]);
    let tag = mac.finalize().into_bytes();
    buf[pt_end..pt_end + MAC_LEN].copy_from_slice(&tag[..MAC_LEN]);

    Ok(())
}

/// Legacy wrapper for tests that operate on standalone `Vec` inputs.
///
/// Prefer `encrypt_ae2_in_place` on the hot path — this one does the extra
/// copy to keep the old test signature.
#[allow(dead_code)]
pub fn encrypt_ae2(password: &str, plaintext: &mut [u8]) -> Result<AesPayload> {
    let plaintext_len = plaintext.len();
    let mut buf = vec![0u8; SALT_LEN + VERIFY_LEN + plaintext_len + MAC_LEN];
    buf[SALT_LEN + VERIFY_LEN..SALT_LEN + VERIFY_LEN + plaintext_len].copy_from_slice(plaintext);
    encrypt_ae2_in_place(password, &mut buf, plaintext_len)?;
    Ok(AesPayload { bytes: buf })
}
