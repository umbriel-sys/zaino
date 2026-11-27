//! Retry policy for JSON-RPC calls.
//!
//! Pure decision logic: no IO, no sleep. The client owns the delay.
//!
//! Only an HTTP status says a validator is too busy. No JSON-RPC error code
//! does: zebrad and zakura answer `-1` for any failed request, invalid
//! parameters among them, so an error object is never asked again.

/// A server at its limit of requests: zebrad's and zakura's answer past 100
/// in flight.
const TOO_MANY_REQUESTS: u16 = 429;

/// A server not taking requests for now.
const SERVICE_UNAVAILABLE: u16 = 503;

/// Does this HTTP status say the validator is up but would not take the
/// call, so that asking again may be answered?
pub(crate) fn is_busy_status(status: u16) -> bool {
    matches!(status, TOO_MANY_REQUESTS | SERVICE_UNAVAILABLE)
}

/// Should we attempt another retry?
pub(crate) fn should_retry(attempt: u32, max_retries: u32) -> bool {
    attempt <= max_retries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_429_and_503_are_busy() {
        assert!(is_busy_status(429));
        assert!(is_busy_status(503));
        for status in [400, 401, 403, 404, 413, 500, 502] {
            assert!(!is_busy_status(status), "{status}");
        }
    }

    #[test]
    fn retry_within_limit() {
        assert!(should_retry(1, 5));
        assert!(should_retry(5, 5));
    }

    #[test]
    fn retry_exhausted() {
        assert!(!should_retry(6, 5));
    }
}
