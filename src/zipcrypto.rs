//! Traditional PKZIP encryption ("ZipCrypto") — decrypt-only.
//!
//! The classic weak crypto that `zip -e`, PKZIP 2.x, and the password
//! dialog of almost every archiver prior to WinZIP AES still produce.
//! Cryptographically broken (known-plaintext attack recovers the key in
//! seconds) — tzip supports it only to **read** vintage archives. The
//! writer never produces it; new encryption goes through AES-256 in
//! `crypto.rs`.
//!
//! Spec: PKWARE APPNOTE.TXT §6.1. The stream cipher state is three
//! 32-bit words K0/K1/K2 driven by CRC-32, and the first 12 bytes of
//! the encrypted body are a header whose last byte is a password-check
//! value (high byte of CRC-32, or high byte of DOS time if the entry
//! uses a streaming data descriptor).
//!
//! A wrong password has a 1/256 chance of passing the single-byte
//! check — that's a property of the format, not the implementation.
//! Downstream decompression will usually fail noisily when this
//! happens; the caller sees garbled bytes otherwise.

use anyhow::{bail, Result};
use crc32fast::Hasher;
use std::io::{self, Read};

/// Internal 3-word state of the PKZIP stream cipher.
#[derive(Clone, Copy)]
struct Keys {
    k0: u32,
    k1: u32,
    k2: u32,
}

impl Keys {
    fn new() -> Self {
        Self {
            k0: 0x1234_5678,
            k1: 0x2345_6789,
            k2: 0x3456_7890,
        }
    }

    /// Mix one plaintext byte into the key schedule. This is the
    /// `update_keys` primitive from APPNOTE §6.1.5.
    fn update(&mut self, byte: u8) {
        self.k0 = crc32_byte(self.k0, byte);
        // The spec says wrapping arithmetic throughout.
        self.k1 = self
            .k1
            .wrapping_add(self.k0 & 0xff)
            .wrapping_mul(0x0808_8405)
            .wrapping_add(1);
        self.k2 = crc32_byte(self.k2, (self.k1 >> 24) as u8);
    }

    /// Current keystream byte. Must be called *before* `update`.
    fn stream_byte(&self) -> u8 {
        let temp = (self.k2 | 2) as u16;
        // The low byte of (temp * (temp ^ 1)) >> 8. Keep everything
        // explicitly `u16` so overflow matches the spec (temp fits in
        // 17 bits before the multiply, but we care only about the low
        // 16 bits of the shifted product — which is byte-sized).
        let product = (temp as u32).wrapping_mul((temp as u32) ^ 1);
        ((product >> 8) & 0xff) as u8
    }

    fn decrypt_byte(&mut self, cipher: u8) -> u8 {
        let plain = cipher ^ self.stream_byte();
        self.update(plain);
        plain
    }
}

fn crc32_byte(crc: u32, byte: u8) -> u32 {
    // Single-byte CRC-32 update, same polynomial `crc32fast` uses. We
    // want byte-at-a-time, so a table-free formulation (or crc32fast's
    // internal API) would be fine, but using the public Hasher API keeps
    // the dep boundary small.
    let mut h = Hasher::new_with_initial(!crc);
    h.update(&[byte]);
    !h.finalize()
}

/// Decrypting adapter: wraps a ciphertext reader, consumes the 12-byte
/// encryption header on construction, verifies the password-check byte,
/// then streams plaintext from `read()`.
pub struct ZipCryptoReader<R: Read> {
    inner: R,
    keys: Keys,
}

impl<R: Read> ZipCryptoReader<R> {
    /// Open a ZipCrypto stream.
    ///
    /// - `password`: user-supplied password bytes.
    /// - `verify`: the expected value of the final header byte. Pass
    ///   `(crc32 >> 24) as u8` when the entry's CRC-32 is known at
    ///   header time (CDR path), or `((dos_time >> 8) & 0xff) as u8`
    ///   when the entry uses a streaming data descriptor (GP bit 3).
    pub fn new(mut inner: R, password: &[u8], verify: u8) -> Result<Self> {
        let mut keys = Keys::new();
        for &b in password {
            keys.update(b);
        }

        let mut header = [0u8; 12];
        inner.read_exact(&mut header)?;
        for b in header.iter_mut() {
            *b = keys.decrypt_byte(*b);
        }
        if header[11] != verify {
            bail!(
                "ZipCrypto password check failed \
                 (header byte {:#04x}, expected {:#04x}) — wrong password",
                header[11],
                verify
            );
        }
        Ok(Self { inner, keys })
    }
}

impl<R: Read> Read for ZipCryptoReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(out)?;
        for b in out[..n].iter_mut() {
            *b = self.keys.decrypt_byte(*b);
        }
        Ok(n)
    }
}

/// Number of bytes ZipCrypto prepends to the compressed body. Callers
/// must subtract this from an entry's `comp_size` before handing the
/// remaining length to the decompressor.
pub const ZIPCRYPTO_HEADER_LEN: u64 = 12;

#[cfg(test)]
mod tests {
    use super::*;

    /// Encrypt side used only to feed the decryptor in tests. Mirrors
    /// the spec's `encrypt_byte`: emit the keystream xor, then update
    /// on the plaintext (just like decrypt — the cipher is symmetric
    /// aside from which value you keep).
    fn encrypt(password: &[u8], plaintext: &[u8], verify: u8) -> Vec<u8> {
        let mut keys = Keys::new();
        for &b in password {
            keys.update(b);
        }
        let mut out = Vec::with_capacity(12 + plaintext.len());
        // 12-byte header: 11 random bytes + the verify byte. "Random"
        // for test determinism is just a known sequence.
        let mut header = [0u8; 12];
        for i in 0..11 {
            header[i] = (i as u8).wrapping_mul(17) ^ 0x5a;
        }
        header[11] = verify;
        for b in header.iter() {
            let plain = *b;
            let cipher = plain ^ keys.stream_byte();
            keys.update(plain);
            out.push(cipher);
        }
        for &b in plaintext {
            let cipher = b ^ keys.stream_byte();
            keys.update(b);
            out.push(cipher);
        }
        out
    }

    #[test]
    fn roundtrip_short() {
        let pw = b"hunter2";
        let plain = b"attack at dawn\n";
        let verify = 0x42;
        let cipher = encrypt(pw, plain, verify);

        let mut reader = ZipCryptoReader::new(&cipher[..], pw, verify).unwrap();
        let mut decoded = Vec::new();
        reader.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, plain);
    }

    #[test]
    fn wrong_password_fails_check() {
        let pw = b"correct";
        let verify = 0xab;
        let cipher = encrypt(pw, b"doesn't matter", verify);
        match ZipCryptoReader::new(&cipher[..], b"wrong", verify) {
            Ok(_) => panic!("wrong password must fail header check"),
            Err(e) => {
                let msg = format!("{}", e);
                assert!(msg.contains("ZipCrypto password check failed"), "msg: {msg}");
            }
        }
    }

    #[test]
    fn empty_body_roundtrips() {
        let pw = b"pw";
        let verify = 0x77;
        let cipher = encrypt(pw, b"", verify);
        assert_eq!(cipher.len(), 12);
        let mut reader = ZipCryptoReader::new(&cipher[..], pw, verify).unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn roundtrip_binary() {
        let pw = b"a password with spaces and symbols !@#";
        // 4 KiB of byte-range noise.
        let plain: Vec<u8> = (0..4096).map(|i| (i & 0xff) as u8).collect();
        let verify = 0x01;
        let cipher = encrypt(pw, &plain, verify);
        let mut reader = ZipCryptoReader::new(&cipher[..], pw, verify).unwrap();
        let mut decoded = Vec::new();
        reader.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, plain);
    }
}
