//! The legacy status a `ZcashService` reports, and its component status.

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle};

/// A legacy component's operational state.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StatusType {
    /// Running initial startup routine.
    Spawning = 0,
    /// Back-end process is currently syncing.
    Syncing = 1,
    /// Process is ready.
    Ready = 2,
    /// Process is busy working.
    Busy = 3,
    /// Running shutdown routine.
    Closing = 4,
    /// Offline.
    Offline = 5,
    /// Non-critical errors.
    RecoverableError = 6,
    /// Critical errors.
    CriticalError = 7,
}

impl From<usize> for StatusType {
    fn from(value: usize) -> Self {
        match value {
            0 => Self::Spawning,
            1 => Self::Syncing,
            2 => Self::Ready,
            3 => Self::Busy,
            4 => Self::Closing,
            5 => Self::Offline,
            6 => Self::RecoverableError,
            _ => Self::CriticalError,
        }
    }
}

impl From<StatusType> for usize {
    fn from(status: StatusType) -> Self {
        status as usize
    }
}

impl fmt::Display for StatusType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Spawning => "Spawning",
            Self::Syncing => "Syncing",
            Self::Ready => "Ready",
            Self::Busy => "Busy",
            Self::Closing => "Closing",
            Self::Offline => "Offline",
            Self::RecoverableError => "RecoverableError",
            Self::CriticalError => "CriticalError",
        })
    }
}

impl StatusType {
    /// The status as a coloured symbol for a terminal.
    pub fn get_status_symbol(&self) -> String {
        let (symbol, color_code) = match self {
            Self::Syncing => ("\u{1F7E1}", "\x1b[33m"),
            Self::Spawning | Self::Busy => ("\u{1F7E1}", "\x1b[36m"),
            Self::Ready => ("\u{1F7E2}", "\x1b[32m"),
            Self::Closing | Self::Offline => ("\u{26AB}", "\x1b[90m"),
            Self::RecoverableError | Self::CriticalError => ("\u{1F534}", "\x1b[31m"),
        };
        format!("{color_code}{symbol}\x1b[0m")
    }

    /// The more severe of the two.
    pub fn combine(self, other: StatusType) -> StatusType {
        match (self, other) {
            (Self::Closing, _) | (_, Self::Closing) => Self::Closing,
            (Self::Offline, _)
            | (_, Self::Offline)
            | (Self::CriticalError, _)
            | (_, Self::CriticalError) => Self::CriticalError,
            (Self::RecoverableError, _) | (_, Self::RecoverableError) => Self::RecoverableError,
            (Self::Spawning, _) | (_, Self::Spawning) => Self::Spawning,
            (Self::Syncing, _) | (_, Self::Syncing) => Self::Syncing,
            _ => Self::Ready,
        }
    }

    /// Whether the component is alive (liveness probe).
    pub fn is_live(self) -> bool {
        !matches!(self, Self::Offline | Self::CriticalError)
    }

    /// Whether the component can serve (readiness probe).
    pub fn is_ready(self) -> bool {
        matches!(self, Self::Ready | Self::Busy)
    }
}

/// A component that reports a [`StatusType`]; it gains [`Liveness`] and
/// [`Readiness`].
pub trait Status {
    /// The component's status now.
    fn status(&self) -> StatusType;
}

/// Liveness probe: is the component alive?
pub trait Liveness {
    /// Whether the component is alive.
    fn is_live(&self) -> bool;
}

/// Readiness probe: can the component serve?
pub trait Readiness {
    /// Whether the component can serve.
    fn is_ready(&self) -> bool;
}

/// Both probes.
pub trait VitalsProbe: Liveness + Readiness {}

impl<T: Liveness + Readiness> VitalsProbe for T {}

impl<T: Status> Liveness for T {
    fn is_live(&self) -> bool {
        self.status().is_live()
    }
}

impl<T: Status> Readiness for T {
    fn is_ready(&self) -> bool {
        self.status().is_ready()
    }
}

/// A shared status cell that logs its transitions under the component's name.
#[derive(Debug, Clone)]
pub struct NamedAtomicStatus {
    name: &'static str,
    inner: Arc<AtomicUsize>,
}

impl NamedAtomicStatus {
    /// A cell for component `name`, at `status`.
    pub fn new(name: &'static str, status: StatusType) -> Self {
        tracing::debug!(component = name, status = %status, "[STATUS] initial");
        Self {
            name,
            inner: Arc::new(AtomicUsize::new(status.into())),
        }
    }

    /// The component's name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The status now.
    pub fn load(&self) -> StatusType {
        StatusType::from(self.inner.load(Ordering::SeqCst))
    }

    /// Replaces the status with `f(current)` atomically; returns it.
    pub fn apply(&self, f: impl Fn(StatusType) -> StatusType) -> StatusType {
        // both arms carry the previous value; the closure never declines
        let (Ok(previous) | Err(previous)) =
            self.inner
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |raw| {
                    Some(f(StatusType::from(raw)).into())
                });
        let old = StatusType::from(previous);
        let new = f(old);
        if old != new {
            tracing::debug!(component = self.name, from = %old, to = %new, "[STATUS] transition");
        }
        new
    }

    /// Sets the status.
    pub fn store(&self, status: StatusType) {
        self.apply(|_| status);
    }
}

impl Status for NamedAtomicStatus {
    fn status(&self) -> StatusType {
        self.load()
    }
}

/// `status` split into the two axes a component reports. `Busy`, like a
/// recoverable error, is a degraded component, not a loaded one.
pub fn component_status(name: &'static str, status: StatusType) -> ComponentStatus {
    let (lifecycle, health) = match status {
        StatusType::Spawning => (Lifecycle::Spawning, Health::Healthy),
        StatusType::Syncing => (Lifecycle::Syncing, Health::Healthy),
        StatusType::Ready => (Lifecycle::Ready, Health::Healthy),
        StatusType::Closing => (Lifecycle::Closing, Health::Healthy),
        StatusType::Offline => (Lifecycle::Offline, Health::Offline),
        StatusType::Busy | StatusType::RecoverableError => {
            (Lifecycle::Syncing, Health::Recoverable)
        }
        StatusType::CriticalError => (Lifecycle::Offline, Health::Critical),
    };
    ComponentStatus::new(ComponentName(name), lifecycle, health)
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

    /// The installed status is `f(current)`, and it is also the return value.
    #[test]
    fn apply_installs_and_returns_the_mapped_status() {
        let status = NamedAtomicStatus::new("test", StatusType::Syncing);
        assert_eq!(status.apply(|_| StatusType::Ready), StatusType::Ready);
        assert_eq!(status.load(), StatusType::Ready);
    }

    /// An identity arm holds the current status: a transition guard.
    #[test]
    fn apply_with_an_identity_arm_holds_the_status() {
        let status = NamedAtomicStatus::new("test", StatusType::Closing);
        let installed = status.apply(|current| match current {
            StatusType::Closing => StatusType::Closing,
            _ => StatusType::Ready,
        });
        assert_eq!(installed, StatusType::Closing);
        assert_eq!(status.load(), StatusType::Closing);
    }

    /// The more severe status wins; `Closing` above all.
    #[test]
    fn combine_keeps_the_more_severe() {
        use StatusType::*;
        assert_eq!(Ready.combine(Syncing), Syncing);
        assert_eq!(Syncing.combine(Spawning), Spawning);
        assert_eq!(Spawning.combine(RecoverableError), RecoverableError);
        assert_eq!(RecoverableError.combine(Offline), CriticalError);
        assert_eq!(CriticalError.combine(Closing), Closing);
        assert_eq!(Ready.combine(Busy), Ready);
    }
}
