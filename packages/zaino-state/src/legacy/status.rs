//! The legacy status a `ZcashService` reports, as a component status.

use zaino_component::{ComponentName, ComponentStatus};
use zaino_status::StatusType;

/// `status` split into the two axes a component reports.
pub fn component_status(name: &'static str, status: StatusType) -> ComponentStatus {
    zaino_chain_store_zainodb::adapter::component_status(ComponentName(name), status)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live and ready as the legacy status was: `Offline` and `CriticalError`
    /// are not live; only `Ready` is ready (`Busy` was never produced).
    #[test]
    fn liveness_and_readiness_carry_over() {
        for (status, live, ready) in [
            (StatusType::Spawning, true, false),
            (StatusType::Syncing, true, false),
            (StatusType::Ready, true, true),
            (StatusType::Closing, true, false),
            (StatusType::Offline, false, false),
            (StatusType::RecoverableError, true, false),
            (StatusType::CriticalError, false, false),
        ] {
            let component = component_status("indexer", status);
            assert_eq!(component.is_live(), live, "{status:?}");
            assert_eq!(component.is_ready(), ready, "{status:?}");
        }
    }
}
