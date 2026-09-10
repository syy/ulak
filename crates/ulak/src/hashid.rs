//! Stable, dependency-free identifiers.
//!
//! FNV-1a/128 is NOT cryptographic — it is a locator hash for remote
//! directory names and compose project suffixes, where accidental
//! collisions are what matters (128 bits is far beyond the 32-bit
//! prototype hash a design review flagged). The permanent identity
//! contract is the random UUID in each workspace manifest, not this hash.

const FNV_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
const FNV_PRIME: u128 = 0x0000000001000000000000000000013b;

pub fn fnv1a128_hex(data: &[u8]) -> String {
    let mut h = FNV_OFFSET;
    for &b in data {
        h ^= b as u128;
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{h:032x}")
}

/// Random v4 UUID from /dev/urandom — the PERMANENT identity stored in
/// each workspace manifest (the hashes above are just locators).
pub fn uuid_v4() -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h = |r: std::ops::Range<usize>| {
        bytes[r]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    Ok(format!(
        "{}-{}-{}-{}-{}",
        h(0..4),
        h(4..6),
        h(6..8),
        h(8..10),
        h(10..16)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_known_vector() {
        // FNV-1a 128 of empty input is the offset basis.
        assert_eq!(fnv1a128_hex(b""), format!("{FNV_OFFSET:032x}"));
        // Deterministic and input-sensitive.
        assert_eq!(fnv1a128_hex(b"abc"), fnv1a128_hex(b"abc"));
        assert_ne!(fnv1a128_hex(b"abc"), fnv1a128_hex(b"abd"));
    }
}
