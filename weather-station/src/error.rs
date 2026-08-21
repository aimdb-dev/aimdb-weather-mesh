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
/// [`StationError`] is `#[non_exhaustive]`, so a mapping written outside this
/// crate needs a wildcard arm and a variant added later lands in it silently.
/// This is the classification such a mapping should match on instead — an FFI
/// layer turning failures into exceptions is the case that needs it, and the
/// three actions do not grow when the variants do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StationErrorKind {
    /// The profile is wrong. Edit it, or have it re-issued.
    Profile,
    /// The broker is unreachable or refused the credential. Fix the
    /// deployment, or re-join for a fresh slot.
    Broker,
    /// The station's own machinery or host. Neither of the above will help.
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
    #[cfg(feature = "sync")]
    #[error("cannot read the station profile at {path}: {reason}")]
    ProfileUnreadable { path: String, reason: String },

    /// The profile file is not a station profile.
    #[cfg(feature = "sync")]
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
    #[cfg(feature = "sync")]
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

            Self::Db(_) => StationErrorKind::Runtime,

            #[cfg(feature = "sync")]
            Self::ProfileUnreadable { .. } | Self::ProfileMalformed { .. } => {
                StationErrorKind::Profile
            }

            #[cfg(feature = "sync")]
            Self::GraphStartTimeout(_) | Self::NoWallClock | Self::Sync(_) => {
                StationErrorKind::Runtime
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        #[cfg(feature = "sync")]
        assert_eq!(StationError::NoWallClock.kind(), StationErrorKind::Runtime);
    }
}
