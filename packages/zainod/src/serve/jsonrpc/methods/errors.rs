//! The JSON-RPC error each failure is served as: zcashd's legacy codes, as
//! zebrad v7 assigns them.

use jsonrpsee::types::{ErrorCode, ErrorObjectOwned};
use zaino_chain::ChainViewError;
use zaino_service::ServiceError;
use zaino_source::{
    FailureMode, GetSpentInfoError, GetStandardFeeError, NonDomainError, SendRawTransactionError,
};

/// `RPC_MISC_ERROR`.
pub(super) const MISC: i32 = -1;
/// `RPC_INVALID_ADDRESS_OR_KEY`.
pub(super) const INVALID_ADDRESS_OR_KEY: i32 = -5;
/// `RPC_INVALID_PARAMETER`.
pub(super) const INVALID_PARAMETER: i32 = -8;
/// `RPC_DESERIALIZATION_ERROR`.
pub(super) const DESERIALIZATION: i32 = -22;

/// An error with `code` and `message`.
pub(super) fn rpc_error(code: i32, message: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(code, message.to_string(), None::<()>)
}

/// Zaino failed, not the request (−32603).
pub(super) fn internal(message: impl std::fmt::Display) -> ErrorObjectOwned {
    rpc_error(ErrorCode::InternalError.code(), message)
}

/// This deployment, or its validator, does not answer the method (−32601).
pub(super) fn not_served(message: impl std::fmt::Display) -> ErrorObjectOwned {
    rpc_error(ErrorCode::MethodNotFound.code(), message)
}

/// A request's parameters contradict each other (−32602).
pub(super) fn invalid_params(message: impl std::fmt::Display) -> ErrorObjectOwned {
    rpc_error(ErrorCode::InvalidParams.code(), message)
}

/// A service failure, where the method gives it no code of its own.
pub(super) fn service_error(error: ServiceError) -> ErrorObjectOwned {
    match error {
        ServiceError::InvalidParameter(message) => rpc_error(INVALID_PARAMETER, message),
        error @ ServiceError::NotServed { .. } => not_served(error),
        ServiceError::Chain(error) => chain_error(error),
        ServiceError::ValidatorUnreachable(error) => unreachable_validator(&error),
        ServiceError::ValidatorRejected(error) => rejection(error.as_ref()),
        error @ ServiceError::InvalidRange { .. } => invalid_params(error),
        error @ (ServiceError::MissingBlock(_)
        | ServiceError::MissingPrevout { .. }
        | ServiceError::AmountOutOfRange { .. }
        | ServiceError::HeightOverflow(_)
        | ServiceError::SpendCountMismatch { .. }
        | ServiceError::MinedAndInMempool(_)
        | ServiceError::MempoolIncoherent
        | ServiceError::MempoolStreamLagged
        | ServiceError::SyncStopped
        | ServiceError::ComponentFailed(_)) => internal(error),
    }
}

/// A chain read's failure.
pub(super) fn chain_error(error: ChainViewError) -> ErrorObjectOwned {
    match error {
        ChainViewError::SourceUnavailable(error) => unreachable_validator(&error),
        error @ (ChainViewError::NotServed(_)
        | ChainViewError::NotBuilt(_)
        | ChainViewError::ValidatorDisabled(_)) => not_served(error),
        error @ ChainViewError::InvalidRange { .. } => invalid_params(error),
        ChainViewError::Rejected(message) => rpc_error(INVALID_PARAMETER, message),
        // `Building`, `Transient`, `Fatal`, and any failure added later
        error => internal(error),
    }
}

/// The validator's own code where it answered with one, else −32603.
fn unreachable_validator(error: &NonDomainError) -> ErrorObjectOwned {
    match error.mode {
        FailureMode::RpcError(code) => match i32::try_from(code) {
            Ok(code) => rpc_error(code, &error.message),
            Err(_) => internal(error),
        },
        FailureMode::Connection
        | FailureMode::Timeout
        | FailureMode::HttpStatus(_)
        | FailureMode::Parse
        | FailureMode::InvalidSourceData
        | FailureMode::Auth => internal(error),
    }
}

/// A validator's refusal: a submission's own code, −32601 for a method the
/// validator lacks, else −8.
fn rejection(error: &(dyn std::error::Error + Send + Sync + 'static)) -> ErrorObjectOwned {
    if let Some(rejection) = error.downcast_ref::<SendRawTransactionError>() {
        return match i32::try_from(rejection.legacy_code()) {
            Ok(code) => rpc_error(code, rejection.message()),
            Err(_) => internal(rejection),
        };
    }
    let unsupported = matches!(
        error.downcast_ref::<GetStandardFeeError>(),
        Some(GetStandardFeeError::Unsupported)
    ) || matches!(
        error.downcast_ref::<GetSpentInfoError>(),
        Some(GetSpentInfoError::Unsupported)
    );
    match unsupported {
        true => not_served(error),
        false => rpc_error(INVALID_PARAMETER, error),
    }
}
