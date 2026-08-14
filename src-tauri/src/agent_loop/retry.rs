use std::time::Duration;

pub(crate) const MAX_RETRIES: usize = 2;

pub(crate) fn delay_for_attempt(attempt: usize, seed: &str) -> Duration {
    let exponent = attempt.min(6) as u32;
    let base = 500_u64.saturating_mul(1_u64 << exponent).min(8_000);
    let jitter = 800 + (stable_hash(seed) % 401);
    Duration::from_millis((base.saturating_mul(jitter) / 1000).min(10_000))
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
