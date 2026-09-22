//! W3C trace-context helpers.

use rand::RngCore;
use smol_str::SmolStr;
use std::fmt::Write;

/// Generate a fresh W3C `traceparent`: `00-<trace-id>-<span-id>-01`, with a
/// random 16-byte trace-id and 8-byte parent-id and the sampled flag set.
///
/// Used as the fallback when an inbound request does not carry a `traceparent`
/// header, so every record on the trace stream has a valid, joinable id.
pub fn new_traceparent() -> SmolStr {
    let mut rng = rand::thread_rng();
    let mut trace_id = [0u8; 16];
    let mut span_id = [0u8; 8];
    rng.fill_bytes(&mut trace_id);
    rng.fill_bytes(&mut span_id);
    // The all-zero value is invalid per the spec; nudge it (astronomically
    // unlikely, but effectively free to guard).
    if trace_id.iter().all(|&b| b == 0) {
        trace_id[0] = 1;
    }
    if span_id.iter().all(|&b| b == 0) {
        span_id[0] = 1;
    }

    // "00-" + 32 + "-" + 16 + "-01" = 55 chars.
    let mut s = String::with_capacity(55);
    s.push_str("00-");
    for b in trace_id {
        let _ = write!(s, "{:02x}", b);
    }
    s.push('-');
    for b in span_id {
        let _ = write!(s, "{:02x}", b);
    }
    s.push_str("-01");
    SmolStr::new(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_and_unique() {
        let tp = new_traceparent();
        let parts: Vec<&str> = tp.split('-').collect();
        assert_eq!(parts.len(), 4, "four dash-separated fields: {tp}");
        assert_eq!(parts[0], "00");
        assert_eq!(parts[1].len(), 32);
        assert_eq!(parts[2].len(), 16);
        assert_eq!(parts[3], "01");
        assert!(parts[1].bytes().all(|c| c.is_ascii_hexdigit()));
        assert!(parts[2].bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(parts[1], "0".repeat(32));
        assert_ne!(parts[2], "0".repeat(16));
        assert_ne!(new_traceparent(), new_traceparent());
    }
}
