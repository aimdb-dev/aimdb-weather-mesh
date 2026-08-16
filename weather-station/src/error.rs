//! What a station can fail at before its record graph runs.
//!
//! These messages are what an operator reads on stderr, so each one names the
//! fix rather than the fault: a rejected credential explains how to get a new
//! slot instead of reporting a CONNACK refusal code.

use thiserror::Error;

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
}
