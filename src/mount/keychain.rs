//! macOS Keychain wrapper — stores + retrieves per-mount AES passwords
//! so the plaintext never appears in the `mount(2)` options string
//! (which is visible via `mount` command and process listings).
//!
//! Each mount gets a UUID-tagged generic password under
//! `service = "tzip-mount"`. The mount options carry only the tag; the
//! FSKit extension uses the same `service + account` tuple to retrieve
//! the password on the Keychain side.

use anyhow::{Context, Result};
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

const SERVICE: &str = "tzip-mount";

/// Generate a fresh UUID-style tag. We don't pull the full `uuid` crate
/// for one call — a 128-bit random hex string from `/dev/urandom` is
/// enough.
pub fn fresh_tag() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    // /dev/urandom is always present on macOS/Linux.
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    }
    let mut s = String::with_capacity(32);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Store a password under `tag`. Replaces any existing entry at the same
/// `(service, account)` pair.
pub fn store(tag: &str, password: &str) -> Result<()> {
    set_generic_password(SERVICE, tag, password.as_bytes())
        .with_context(|| format!("store password for tag {} in Keychain", tag))
}

/// Retrieve a password previously stored under `tag`.
#[allow(dead_code)]
pub fn retrieve(tag: &str) -> Result<String> {
    let bytes = get_generic_password(SERVICE, tag)
        .with_context(|| format!("retrieve password for tag {} from Keychain", tag))?;
    String::from_utf8(bytes).context("Keychain password is not valid UTF-8")
}

/// Delete the password entry under `tag`. Not an error if absent.
pub fn delete(tag: &str) -> Result<()> {
    match delete_generic_password(SERVICE, tag) {
        Ok(()) => Ok(()),
        Err(e) => {
            // `errSecItemNotFound` (-25300) is fine — nothing to delete.
            if e.code() == -25300 {
                Ok(())
            } else {
                Err(e).with_context(|| format!("delete Keychain entry for tag {}", tag))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard that deletes the Keychain entry on drop so a failed test
    /// doesn't leave garbage behind.
    struct KeychainCleanup(String);
    impl Drop for KeychainCleanup {
        fn drop(&mut self) {
            let _ = delete(&self.0);
        }
    }

    #[test]
    fn store_retrieve_delete_roundtrip() {
        let tag = fresh_tag();
        let _cleanup = KeychainCleanup(tag.clone());

        store(&tag, "s3cr3t!").expect("store failed");
        let got = retrieve(&tag).expect("retrieve failed");
        assert_eq!(got, "s3cr3t!");
        delete(&tag).expect("delete failed");

        // Retrieve after delete should fail.
        assert!(retrieve(&tag).is_err());
    }

    #[test]
    fn delete_absent_is_ok() {
        let tag = fresh_tag();
        delete(&tag).expect("deleting a non-existent entry should be a no-op");
    }

    #[test]
    fn fresh_tag_is_unique_enough() {
        let a = fresh_tag();
        let b = fresh_tag();
        assert_ne!(a, b, "two fresh tags collided");
        assert_eq!(a.len(), 32);
    }
}
