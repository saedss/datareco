//! New GUIDs and MBR disk signatures. They only need to be unique, not
//! secret: a hash of the time, the process and a counter does.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A random (version 4) GUID, in the on-disk byte order.
pub(crate) fn new_guid() -> [u8; 16] {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut hash = Sha256::new();
    hash.update(now.as_secs().to_le_bytes());
    hash.update(now.subsec_nanos().to_le_bytes());
    hash.update(std::process::id().to_le_bytes());
    hash.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let digest = hash.finalize();
    let mut guid = [0u8; 16];
    guid.copy_from_slice(&digest[..16]);
    // The version is the top of the third group (stored little-endian, so
    // byte 7); the variant the top of byte 8.
    guid[7] = (guid[7] & 0x0F) | 0x40;
    guid[8] = (guid[8] & 0x3F) | 0x80;
    guid
}

/// A new MBR disk signature (never 0, which means "none").
pub(crate) fn new_signature() -> u32 {
    let guid = new_guid();
    u32::from_le_bytes([guid[0], guid[1], guid[2], guid[3]]).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids_differ_and_say_version_4() {
        let (a, b) = (new_guid(), new_guid());
        assert_ne!(a, b);
        assert_eq!(a[7] >> 4, 4);
        assert_eq!(a[8] >> 6, 0b10);
        assert_ne!(new_signature(), 0);
    }
}
