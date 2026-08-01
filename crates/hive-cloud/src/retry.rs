//! Retry / failover SAFETY classification (Fluid hardening #6 / #7 / #8).
//!
//! The mesh router fails a request over to another transport (iroh→HTTP) or peer
//! when an attempt errors. That is only safe when re-attempting cannot duplicate a
//! side effect or replay/garble a response. This module is the pure, testable
//! policy the router consults before any RE-attempt (the first attempt is never a
//! retry). It composes three signals:
//!
//!   * #6 retry-safety  — idempotent method, or an explicit idempotency key.
//!   * #7 body replay   — the request body must be fully buffered to be re-sent.
//!   * #8 response start — once response bytes have begun flowing to the client,
//!                         a transparent retry is impossible.
//!
//! Conservative by design: when in doubt it REFUSES the retry (returns a controlled
//! error) rather than risk executing a POST twice. The first attempt always runs.

/// Whether the request body can be re-sent on a retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyReplay {
    /// Fully buffered in memory — safe to re-send.
    Buffered,
    /// Not replayable (too large to buffer, or a consumed stream).
    NonReplayable,
}

/// Whether the client has started receiving the response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseState {
    NotStarted,
    Started,
}

/// The decision + a machine-readable reason (for logs/metrics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryDecision {
    pub allowed: bool,
    pub reason: &'static str,
}

/// HTTP methods that are idempotent by definition (RFC 9110) — safe to repeat.
pub fn is_idempotent(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "OPTIONS" | "PUT" | "DELETE" | "TRACE"
    )
}

/// Decide whether an ALREADY-ATTEMPTED request may be retried / failed over.
/// (The caller must only invoke this for the 2nd+ attempt — the first always runs.)
pub fn can_retry(
    method: &str,
    has_idempotency_key: bool,
    body: BodyReplay,
    response: ResponseState,
) -> RetryDecision {
    // Once bytes are on the wire to the client, a transparent retry is impossible.
    if response == ResponseState::Started {
        return RetryDecision {
            allowed: false,
            reason: "response_already_started",
        };
    }
    // Can't re-send a body we couldn't buffer.
    if body == BodyReplay::NonReplayable {
        return RetryDecision {
            allowed: false,
            reason: "body_not_replayable",
        };
    }
    // A non-idempotent method without an idempotency key might double-execute.
    if !is_idempotent(method) && !has_idempotency_key {
        return RetryDecision {
            allowed: false,
            reason: "non_idempotent_no_key",
        };
    }
    RetryDecision {
        allowed: true,
        reason: "safe",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use BodyReplay::*;
    use ResponseState::*;

    #[test]
    fn idempotent_methods() {
        for m in ["GET", "head", "OPTIONS", "Put", "DELETE", "TRACE"] {
            assert!(is_idempotent(m), "{m} should be idempotent");
        }
        for m in ["POST", "patch", "CONNECT"] {
            assert!(!is_idempotent(m), "{m} should not be idempotent");
        }
    }

    #[test]
    fn get_can_retry_before_response() {
        assert!(can_retry("GET", false, Buffered, NotStarted).allowed);
    }

    #[test]
    fn post_without_key_cannot_retry() {
        let d = can_retry("POST", false, Buffered, NotStarted);
        assert!(!d.allowed);
        assert_eq!(d.reason, "non_idempotent_no_key");
    }

    #[test]
    fn post_with_idempotency_key_can_retry_before_response() {
        assert!(can_retry("POST", true, Buffered, NotStarted).allowed);
    }

    #[test]
    fn never_retry_after_response_started() {
        // even a GET is unsafe to transparently retry once the client sees bytes
        let d = can_retry("GET", true, Buffered, Started);
        assert!(!d.allowed);
        assert_eq!(d.reason, "response_already_started");
    }

    #[test]
    fn non_replayable_body_cannot_retry() {
        let d = can_retry("PUT", true, NonReplayable, NotStarted);
        assert!(!d.allowed);
        assert_eq!(d.reason, "body_not_replayable");
    }
}
