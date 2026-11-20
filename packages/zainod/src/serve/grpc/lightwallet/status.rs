//! A read's failure as lightwalletd clients see it: the gRPC status, and for
//! a submission the validator's own code and message.

use zaino_chain::ChainViewError;
use zaino_service::ServiceError;
use zaino_source::{FailureMode, GetStandardFeeError, NonDomainError, SendRawTransactionError};

/// How a failed validator read is worded on the wire.
const BACKING_NODE: &str = "InternalServerError: error receiving data from backing node: \
    critical error in backing block source: ";

/// What kind of failure a refusal is, and so its gRPC code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// Zaino or its validator is not working.
    Internal,
    /// The snapshot read holds data that cannot both be true.
    InvalidSnapshot,
    /// Not right now: a later attempt is answered.
    Unavailable,
    /// The request is wrong and fails the same way unchanged.
    InvalidArgument,
    /// This deployment does not serve it.
    Unimplemented,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Internal => "internal server error",
            Self::InvalidSnapshot => "invalid snapshot",
            Self::Unavailable => "unavailable",
            Self::InvalidArgument => "invalid argument",
            Self::Unimplemented => "unimplemented",
        }
    }
}

/// A read that was not answered.
#[derive(Debug)]
pub(super) struct Refusal {
    pub(super) kind: Kind,
    pub(super) message: String,
    /// The validator's legacy code and message, for a rejected submission.
    pub(super) rejection: Option<(i64, String)>,
    /// Whether the validator could not be reached (not an answer).
    pub(super) unreachable: bool,
}

impl Refusal {
    pub(super) fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            rejection: None,
            unreachable: false,
        }
    }

    /// A request the validator's rules reject.
    pub(super) fn rejected_query(reason: impl std::fmt::Display) -> Self {
        Self::new(
            Kind::InvalidArgument,
            format!("{BACKING_NODE}validator rejected the query: {reason}"),
        )
    }

    /// A read the validator answered with a refusal, as its port names it.
    pub(super) fn validator_refused(refusal: impl std::fmt::Display) -> Self {
        Self::new(
            Kind::Internal,
            format!("{BACKING_NODE}validator rejected the query: {refusal}"),
        )
    }

    /// A parameter outside what the validator's port accepts.
    pub(super) fn validator_input(reason: impl std::fmt::Display) -> Self {
        Self::new(Kind::Internal, format!("{BACKING_NODE}{reason}"))
    }

    fn unreachable_validator(error: &NonDomainError) -> Self {
        Self {
            unreachable: matches!(error.mode, FailureMode::Connection | FailureMode::Timeout),
            ..Self::new(
                Kind::Internal,
                format!("{BACKING_NODE}validator unreachable: {error}"),
            )
        }
    }

    /// A service read's failure.
    pub(super) fn of_service(error: ServiceError) -> Self {
        match error {
            ServiceError::InvalidParameter(message) => Self::new(Kind::InvalidArgument, message),
            ServiceError::Chain(error) => Self::of_chain(error),
            ServiceError::ValidatorUnreachable(error) => Self::unreachable_validator(&error),
            ServiceError::ValidatorRejected(error) => Self::of_rejection(error.as_ref()),
            error @ ServiceError::NotServed { .. } => {
                Self::new(Kind::Unimplemented, error.to_string())
            }
            error @ ServiceError::InvalidRange { .. } => {
                Self::new(Kind::InvalidArgument, error.to_string())
            }
            error @ (ServiceError::MempoolIncoherent
            | ServiceError::MempoolStreamLagged
            | ServiceError::SyncStopped) => Self::new(Kind::Unavailable, error.to_string()),
            error @ (ServiceError::MissingBlock(_)
            | ServiceError::MissingPrevout { .. }
            | ServiceError::AmountOutOfRange { .. }
            | ServiceError::HeightOverflow(_)
            | ServiceError::SpendCountMismatch { .. }
            | ServiceError::MinedAndInMempool(_)) => Self::new(Kind::Internal, error.to_string()),
        }
    }

    /// The validator's refusal, by the port error it answered with.
    fn of_rejection(error: &(dyn std::error::Error + Send + Sync + 'static)) -> Self {
        if let Some(rejection) = error.downcast_ref::<SendRawTransactionError>() {
            return Self {
                rejection: Some((rejection.legacy_code(), rejection.message().to_owned())),
                ..Self::new(
                    Kind::Internal,
                    format!(
                        "{BACKING_NODE}validator rejected the transaction: {}",
                        rejection.message()
                    ),
                )
            };
        }
        if let Some(GetStandardFeeError::Unsupported) = error.downcast_ref::<GetStandardFeeError>()
        {
            return Self::new(
                Kind::Unimplemented,
                format!("{BACKING_NODE}validator rejected the query: {error}"),
            );
        }
        Self::validator_refused(error)
    }

    /// A chain view read's failure.
    pub(super) fn of_chain(error: ChainViewError) -> Self {
        match error {
            ChainViewError::Rejected(message) => Self::rejected_query(message),
            ChainViewError::SourceUnavailable(error) => Self::unreachable_validator(&error),
            error @ ChainViewError::InvalidRange { .. } => Self::rejected_query(error),
            error @ (ChainViewError::NotServed(_)
            | ChainViewError::NotBuilt(_)
            | ChainViewError::ValidatorDisabled(_)) => {
                Self::new(Kind::Unimplemented, error.to_string())
            }
            error @ (ChainViewError::Building { .. } | ChainViewError::Transient(_)) => {
                Self::new(Kind::Unavailable, error.to_string())
            }
            error @ ChainViewError::Fatal(_) => Self::new(Kind::Internal, error.to_string()),
            other => Self::new(Kind::Internal, format!("chain view read failed: {other}")),
        }
    }

    /// A block the snapshot's own range should hold and does not.
    pub(super) fn missing_block(height: u32) -> Self {
        Self::new(
            Kind::Internal,
            format!("InternalServerError: hole in validator database, missing block {height}"),
        )
    }

    /// The refusal as one line, its kind first.
    pub(super) fn line(&self) -> String {
        format!("{}: {}", self.kind.name(), self.message)
    }

    /// The gRPC status clients receive.
    pub(super) fn status(self) -> tonic::Status {
        match self.kind {
            Kind::Internal => tonic::Status::internal(self.message),
            Kind::InvalidSnapshot => tonic::Status::failed_precondition(self.message),
            Kind::Unavailable => tonic::Status::unavailable(self.message),
            Kind::InvalidArgument => tonic::Status::invalid_argument(self.message),
            Kind::Unimplemented => tonic::Status::unimplemented(self.message),
        }
    }

    /// `Unavailable` where the validator could not be reached, else the
    /// refusal's own status.
    pub(super) fn status_unavailable_if_unreachable(self) -> tonic::Status {
        match self.unreachable {
            true => tonic::Status::unavailable(format!("Chain index error: {}", self.line())),
            false => self.status(),
        }
    }
}

/// A service read's failure as its gRPC status.
pub(super) fn of_service(error: ServiceError) -> tonic::Status {
    Refusal::of_service(error).status()
}

/// A chain view read's failure as its gRPC status.
pub(super) fn of_chain(error: ChainViewError) -> tonic::Status {
    Refusal::of_chain(error).status()
}
