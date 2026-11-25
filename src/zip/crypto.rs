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
use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha1::Sha1;
use std::io::{Read, Write};

type HmacSha1 = Hmac<Sha1>;
type Aes256Ctr = ctr::Ctr128LE<aes::Aes256>;

pub const SALT_LEN: usize = 16;
pub const VERIFY_LEN: usize = 2;
pub const MAC_LEN: usize = 10;
pub const KEY_LEN: usize = 32; // AES-256
pub const MAC_KEY_LEN: usize = 32; // HMAC-SHA1 key size per WinZIP AES

/// Overhead bytes added to a plaintext body when it is AE-2 encrypted.
pub const AE2_OVERHEAD: usize = SALT_LEN + VERIFY_LEN + MAC_LEN;

/// Wrap a plaintext payload in a WinZIP AES-256 AE-2 envelope.
///
/// Used by both the create-mode pipeline for user entries (via the
/// in-place single-alloc path) and — new in the ChatGPT review
/// fix pass — by the sentinel emitters for `.tzip/*.v1` index
/// entries. Pre-fix, index payloads were written with
/// `encrypted: false` on AES archives, letting any reader
/// enumerate entry names, index terms, facet values, and columnar
/// contents without the password. The index is content-derived;
/// if the content is secret, the index is too.
///
/// Returns a buffer of length `plaintext.len() + AE2_OVERHEAD`.
pub fn wrap_ae2(password: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(plaintext.len() + AE2_OVERHEAD);
    let prefix = SALT_LEN + VERIFY_LEN;
    buf.resize(plaintext.len() + AE2_OVERHEAD, 0);
    buf[prefix..prefix + plaintext.len()].copy_from_slice(plaintext);
    encrypt_ae2_in_place(password, &mut buf, plaintext.len())?;
    Ok(buf)
}

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

    let keys = derive_keys(password, salt);

    // Fill the salt + verifier prefix in place.
    buf[..SALT_LEN].copy_from_slice(salt);
    buf[SALT_LEN..SALT_LEN + VERIFY_LEN].copy_from_slice(&keys.verify);

    let mut cipher = ctr_cipher(&keys.enc);

    let pt_start = SALT_LEN + VERIFY_LEN;
    let pt_end = pt_start + plaintext_len;
    cipher.apply_keystream(&mut buf[pt_start..pt_end]);

    let mut mac = HmacSha1::new_from_slice(&keys.mac).expect("hmac key size");
    mac.update(&buf[pt_start..pt_end]);
    let tag = mac.finalize().into_bytes();
    buf[pt_end..pt_end + MAC_LEN].copy_from_slice(&tag[..MAC_LEN]);

    Ok(())
}

/// PBKDF2-derived material for a given (password, salt) pair.
struct AesKeys {
    enc: [u8; KEY_LEN],
    mac: [u8; MAC_KEY_LEN],
    verify: [u8; VERIFY_LEN],
}

fn derive_keys(password: &str, salt: &[u8; SALT_LEN]) -> AesKeys {
    let mut derived = [0u8; KEY_LEN + MAC_KEY_LEN + VERIFY_LEN];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), salt, 1000, &mut derived);
    AesKeys {
        enc: derived[..KEY_LEN].try_into().unwrap(),
        mac: derived[KEY_LEN..KEY_LEN + MAC_KEY_LEN].try_into().unwrap(),
        verify: derived[KEY_LEN + MAC_KEY_LEN..].try_into().unwrap(),
    }
}

fn ctr_cipher(enc_key: &[u8; KEY_LEN]) -> Aes256Ctr {
    // WinZIP AES: 128-bit LE counter starting at 1. IV = [1, 0, 0, ...]
    // so the first plaintext block is encrypted with counter=1 directly.
    let mut iv = [0u8; 16];
    iv[0] = 1;
    Aes256Ctr::new(enc_key.into(), (&iv).into())
}

/// Constant-time byte-slice equality.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Streaming AES-256 AE-2 decrypt.
///
/// Reads from `src` in order:
///   1. `SALT_LEN` bytes of salt,
///   2. `VERIFY_LEN` bytes of password verifier,
///   3. `ciphertext_len` bytes of ciphertext,
///   4. `MAC_LEN` bytes of HMAC-SHA1 tag.
///
/// On a wrong password the verifier mismatches and we fail *before* any
/// plaintext is written to `dst`. On MAC mismatch we fail *after* the
/// stream is through — the caller must treat any bytes already written to
/// `dst` as untrusted and discard them.
pub fn decrypt_ae2<R: Read + ?Sized, W: Write + ?Sized>(
    password: &str,
    src: &mut R,
    ciphertext_len: u64,
    dst: &mut W,
) -> Result<u64> {
    if password.is_empty() {
        bail!("empty password");
    }
    let mut salt = [0u8; SALT_LEN];
    src.read_exact(&mut salt).context("read AES salt")?;
    let mut got_verify = [0u8; VERIFY_LEN];
    src.read_exact(&mut got_verify).context("read AES verifier")?;

    let keys = derive_keys(password, &salt);
    if !ct_eq(&got_verify, &keys.verify) {
        bail!("wrong password (AES-256 verifier mismatch)");
    }

    let mut cipher = ctr_cipher(&keys.enc);
    let mut mac = HmacSha1::new_from_slice(&keys.mac).expect("hmac key size");

    let mut remaining = ciphertext_len;
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        src.read_exact(&mut buf[..want])
            .context("read AES ciphertext")?;
        mac.update(&buf[..want]);
        cipher.apply_keystream(&mut buf[..want]);
        dst.write_all(&buf[..want]).context("write plaintext")?;
        written += want as u64;
        remaining -= want as u64;
    }

    let mut got_mac = [0u8; MAC_LEN];
    src.read_exact(&mut got_mac).context("read AES MAC")?;
    let want_mac = mac.finalize().into_bytes();
    if !ct_eq(&got_mac, &want_mac[..MAC_LEN]) {
        bail!("AES-256 HMAC mismatch — archive is tampered or corrupted");
    }
    Ok(written)
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
