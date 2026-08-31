//! What a station can fail at before its record graph runs.
//!
//! These messages are what an operator reads on stderr, so each one names the
//! fix rather than the fault: a rejected credential explains how to get a new
//! slot instead of reporting a CONNACK refusal code.

use alloc::string::String;

use thiserror::Error;

/// What a caller does about a failure: fix the file, fix the deployment, or
/// neither.
///
/// Match on this rather than on [`StationError`], which is `#[non_exhaustive]`
/// — an FFI layer turning failures into exceptions is the case that needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StationErrorKind {
    /// The profile is wrong. Edit it, or have it re-issued.
    Profile,
    /// The broker is unreachable or refused the credential. Fix the
    /// deployment, or re-join for a fresh slot.
    Broker,
    /// The station is closed: its runtime thread is gone, or this is a forked
    /// child that never had one. Nothing reaches the mesh through this handle
    /// again — open another station.
    ///
    /// Distinct from [`Runtime`](StationErrorKind::Runtime) because it is the
    /// one terminal failure a caller can *expect*: it is what every in-flight
    /// publish gets during shutdown, and a caller that treats it as an error
    /// worth reporting will report it once per sensor thread on every clean
    /// exit.
    Closed,
    /// The station's own machinery or host. None of the above will help.
    Runtime,
}

/// A station failed to join the mesh.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StationError {
    /// The profile was issued for a format this station does not understand.
    #[error(
        "unsupported profile_version {found} (this station understands {expected}) — \
         update the station, or have the profile re-issued for version {expected}"
    )]
    UnsupportedProfileVersion { found: u64, expected: u64 },

    /// `station_id` is not the mesh's slot-scoped identity format.
    #[error(
        "station_id '{0}' is not of the form slot-<n> — \
         is this profile from a weather-mesh deployment?"
    )]
    MalformedStationId(String),

    /// The `[broker]` table's URL cannot be used to reach a broker.
    #[error("{0}")]
    BrokerUrl(String),

    /// The broker refused the station's credential.
    #[error(
        "the broker rejected this station's credential ({0}).\n  \
         The slot was likely revoked (silent for 30 days, or by the operator).\n  \
         Re-join the mesh to get a fresh slot."
    )]
    CredentialRejected(String),

    /// The broker could not be reached at all.
    #[error("cannot reach the broker at {url}: {reason}")]
    BrokerUnreachable { url: String, reason: String },

    /// The pre-flight CONNECT never got an answer.
    #[error("timed out connecting to the broker at {0}")]
    BrokerTimeout(String),

    /// The record graph failed to build or run.
    #[error(transparent)]
    Db(#[from] aimdb_core::DbError),

    /// The profile file could not be read.
    #[cfg(feature = "std")]
    #[error("cannot read the station profile at {path}: {reason}")]
    ProfileUnreadable { path: String, reason: String },

    /// The profile file is not a station profile.
    #[cfg(feature = "std")]
    #[error("the station profile at {path} is malformed: {reason}")]
    ProfileMalformed { path: String, reason: String },

    /// The graph was attached but never started publishing.
    ///
    /// Producing before the outbound links are live loses the value silently
    /// (the slot's buffer is a broadcast, so a reader that subscribes later
    /// never sees it), so [`StationHandle`](crate::StationHandle) refuses to
    /// hand back a station it cannot prove is pumping.
    #[cfg(feature = "sync")]
    #[error("the record graph did not start within {0:?}")]
    GraphStartTimeout(core::time::Duration),

    /// The system clock is unusable, so readings would carry no timestamp.
    #[cfg(feature = "std")]
    #[error(
        "the system clock is before the Unix epoch, so readings would carry no \
         usable timestamp — set the clock (or NTP) before starting the station"
    )]
    NoWallClock,

    /// The sync runtime bridge failed.
    #[cfg(feature = "sync")]
    #[error(transparent)]
    Sync(#[from] aimdb_sync::SyncError),
}

impl StationError {
    /// Classify the failure by what the caller can do about it.
    ///
    /// A malformed broker URL is a [`Profile`](StationErrorKind::Profile)
    /// fault, not a broker one: the broker was never reached, and the fix is
    /// in the file.
    pub fn kind(&self) -> StationErrorKind {
        match self {
            Self::UnsupportedProfileVersion { .. }
            | Self::MalformedStationId(_)
            | Self::BrokerUrl(_) => StationErrorKind::Profile,

            Self::CredentialRejected(_)
            | Self::BrokerUnreachable { .. }
            | Self::BrokerTimeout(_) => StationErrorKind::Broker,

            // Delegated, not flattened: `DbError` already classifies this, and
            // collapsing it here is what used to force the FFI layers to
            // re-check `is_closed()` before every publish to recover the
            // distinction — a second check that could disagree with the one the
            // publish itself performs.
            Self::Db(e) => Self::from_db_kind(e.kind()),

            #[cfg(feature = "std")]
            Self::ProfileUnreadable { .. } | Self::ProfileMalformed { .. } => {
                StationErrorKind::Profile
            }

            #[cfg(feature = "std")]
            Self::NoWallClock => StationErrorKind::Runtime,

            #[cfg(feature = "sync")]
            Self::GraphStartTimeout(_) => StationErrorKind::Runtime,

            // `SyncError::RuntimeShutdown` and `ForkedChild` are already
            // `DbErrorKind::Closed`; this is the path a publish after shutdown
            // actually takes.
            #[cfg(feature = "sync")]
            Self::Sync(e) => Self::from_db_kind(e.kind()),
        }
    }

    /// The one place aimdb's classification is narrowed to a station's.
    fn from_db_kind(kind: aimdb_core::DbErrorKind) -> StationErrorKind {
        match kind {
            aimdb_core::DbErrorKind::Closed => StationErrorKind::Closed,
            _ => StationErrorKind::Runtime,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A publish after shutdown classifies as `Closed`, not `Runtime`.
    ///
    /// This is what lets an FFI layer report a closed station without checking
    /// `is_closed()` before every publish — a second check, on a different
    /// object, that could disagree with the one `SyncProducer::set` performs.
    /// Both doors used to carry it; deleting it is only sound while this holds.
    #[cfg(feature = "sync")]
    #[test]
    fn a_publish_after_shutdown_is_closed_not_runtime() {
        assert_eq!(
            StationError::Sync(aimdb_sync::SyncError::RuntimeShutdown).kind(),
            StationErrorKind::Closed
        );
        // A forked child never had a runtime thread — same terminal answer, and
        // a message that says which of the two it was.
        assert_eq!(
            StationError::Sync(aimdb_sync::SyncError::ForkedChild).kind(),
            StationErrorKind::Closed
        );
    }

    /// The rest of aimdb's kinds stay `Runtime`: `Closed` is the one a caller
    /// treats differently, and widening it would make every transport hiccup
    /// look terminal.
    #[cfg(feature = "sync")]
    #[test]
    fn other_db_failures_stay_runtime() {
        assert_eq!(
            StationError::Sync(aimdb_sync::SyncError::SetTimeout).kind(),
            StationErrorKind::Runtime
        );
    }

    /// The point of `kind` is that it is exhaustive *here*, so a variant added
    /// later is a compile error in this file rather than a silent
    /// reclassification at every FFI boundary.
    #[test]
    fn the_three_kinds_split_by_what_fixes_them() {
        assert_eq!(
            StationError::MalformedStationId("x".into()).kind(),
            StationErrorKind::Profile
        );
        assert_eq!(
            StationError::BrokerUrl("x".into()).kind(),
            StationErrorKind::Profile
        );
        assert_eq!(
            StationError::BrokerTimeout("x".into()).kind(),
            StationErrorKind::Broker
        );
        #[cfg(feature = "std")]
        assert_eq!(StationError::NoWallClock.kind(), StationErrorKind::Runtime);
    }
}
