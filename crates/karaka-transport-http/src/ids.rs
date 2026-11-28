//! Random ids, as `crypto.randomUUID` makes them: version 4.

use ring::rand::{SecureRandom, SystemRandom};

/// A random version-4 UUID, lowercase, as `randomUUID` writes it.
pub fn random_uuid() -> String {
    let mut bytes = [0_u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("the system's random source is available");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}
