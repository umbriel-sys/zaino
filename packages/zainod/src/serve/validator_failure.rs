//! How a validator read that got no answer is worded: the same on every
//! interface Zaino serves.

use zaino_source::{FailureMode, NonDomainError};

/// How a validator read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidatorFailure {
    /// No connection could be made.
    Unreachable,
    /// No answer came in the time allowed.
    TimedOut,
    /// The validator is up but would not take the request for now.
    Busy,
    /// The validator refused Zaino's credentials.
    Unauthorized,
    /// The validator answered, and the answer was a failure or unusable.
    Failed,
}

impl ValidatorFailure {
    /// The failure `error` is.
    pub(crate) fn of(error: &NonDomainError) -> Self {
        match error.mode {
            FailureMode::Connection => Self::Unreachable,
            FailureMode::Timeout => Self::TimedOut,
            FailureMode::Busy => Self::Busy,
            FailureMode::Auth => Self::Unauthorized,
            FailureMode::HttpStatus(_)
            | FailureMode::RpcError(_)
            | FailureMode::Parse
            | FailureMode::InvalidSourceData => Self::Failed,
        }
    }

    fn phrase(self) -> &'static str {
        match self {
            Self::Unreachable => "validator unreachable",
            Self::TimedOut => "validator did not answer in time",
            Self::Busy => "validator busy",
            Self::Unauthorized => "validator refused Zaino's credentials",
            Self::Failed => "validator read failed",
        }
    }

    /// `error` as clients read it: how the read failed, then its cause.
    pub(crate) fn describe(error: &NonDomainError) -> String {
        format!("{}: {error}", Self::of(error).phrase())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use zaino_rpc::RpcError;

    use super::*;

    /// One real failure of each kind, with how it is described.
    pub(crate) fn failures() -> Vec<(NonDomainError, ValidatorFailure, &'static str)> {
        let saturated = RpcError::Saturated {
            waited: std::time::Duration::from_secs(30),
            max_in_flight: 64,
        };
        let busy = RpcError::Busy {
            attempts: 6,
            status: 429,
        };
        vec![
            (
                NonDomainError::new(FailureMode::Connection, "connection refused"),
                ValidatorFailure::Unreachable,
                "validator unreachable: connection refused",
            ),
            (
                NonDomainError::from(saturated),
                ValidatorFailure::TimedOut,
                "validator did not answer in time: no request slot to the validator came free \
                 in 30s: 64 requests were in flight the whole time",
            ),
            (
                NonDomainError::from(busy),
                ValidatorFailure::Busy,
                "validator busy: the validator stayed busy (http status 429) through 6 attempt(s)",
            ),
            (
                NonDomainError::from(RpcError::Status(401)),
                ValidatorFailure::Unauthorized,
                "validator refused Zaino's credentials: http status 401",
            ),
            (
                NonDomainError::from(RpcError::Status(500)),
                ValidatorFailure::Failed,
                "validator read failed: http status 500",
            ),
        ]
    }

    #[test]
    fn each_failure_says_how_it_failed_and_why() {
        for (error, failure, described) in failures() {
            assert_eq!(ValidatorFailure::of(&error), failure);
            assert_eq!(ValidatorFailure::describe(&error), described);
        }
    }
}
