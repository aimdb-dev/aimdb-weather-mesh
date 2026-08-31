//! The profile tables every mesh station shares, and the two checks that turn
//! a parsed profile into a mesh identity.
//!
//! A station composes these into its own `StationProfile` rather than
//! inheriting one, so a station with an extra table (`[knx]`, say) does not
//! need this crate to know the table exists:
//!
//! ```
//! # use serde::Deserialize;
//! # use weather_station::{AppProfile, BrokerProfile};
//! #[derive(Debug, Deserialize)]
//! struct StationProfile {
//!     station_id: String,
//!     broker: BrokerProfile,
//!     app: AppProfile,
//! }
//! ```
//!
//! [`load_profile`] reads that struct off disk, version gate included.
//!
//! Unknown fields are ignored throughout, so the provisioning service can
//! extend the format without a version bump.

use alloc::string::{String, ToString};

use serde::Deserialize;

use crate::StationError;

/// The profile format this crate understands.
///
/// A bump means stations have to be updated, not that an older profile can be
/// read leniently — see [`check_profile_version`].
pub const PROFILE_VERSION: u64 = 1;

/// The profile's `[broker]` table: where the station publishes, and the
/// credential the mesh issued for its slot.
#[derive(Debug, Deserialize)]
pub struct BrokerProfile {
    /// `mqtt[s]://host[:port]`.
    pub url: String,
    pub username: String,
    pub password: String,
}

/// The profile's `[app]` table: station name plus the coordinates the mesh
/// published for it, coarsened to two decimals (~1 km).
///
/// The coordinates are optional — a hand-written profile may omit them, and a
/// station with no location-dependent behaviour ignores them.
#[derive(Debug, Deserialize)]
pub struct AppProfile {
    pub name: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

/// Reject a profile issued for a format this station cannot honour.
pub fn check_profile_version(found: u64) -> Result<(), StationError> {
    if found == PROFILE_VERSION {
        Ok(())
    } else {
        Err(StationError::UnsupportedProfileVersion {
            found,
            expected: PROFILE_VERSION,
        })
    }
}

/// Station identities are slot-scoped: `station_id = "slot-<n>"` maps onto the
/// hub's pool records `station.<n>.*`.
pub fn slot_from_station_id(station_id: &str) -> Result<u16, StationError> {
    station_id
        .strip_prefix("slot-")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| StationError::MalformedStationId(station_id.to_string()))
}

/// Read a station profile from a `station.toml` path.
///
/// `T` is the station's own profile struct — the mesh tables composed with
/// whatever else it carries, as above. The version gate runs before `T` is
/// deserialized, so a station cannot reach its own tables out of a profile the
/// mesh was not issued for. That is what makes it the gate every door shares
/// rather than one each station remembers to call.
///
/// Both failures name the path, because the operator reading them has to find
/// the file.
#[cfg(feature = "std")]
pub fn load_profile<T: serde::de::DeserializeOwned>(
    path: impl AsRef<std::path::Path>,
) -> Result<T, StationError> {
    /// The one field read before the station's own tables are.
    #[derive(Deserialize)]
    struct VersionProbe {
        profile_version: u64,
    }

    fn malformed(path: &std::path::Path, e: toml::de::Error) -> StationError {
        StationError::ProfileMalformed {
            path: path.display().to_string(),
            reason: e.to_string(),
        }
    }

    let path = path.as_ref();
    let raw = std::fs::read_to_string(path).map_err(|e| StationError::ProfileUnreadable {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;

    let probe: VersionProbe = toml::from_str(&raw).map_err(|e| malformed(path, e))?;
    check_profile_version(probe.profile_version)?;

    toml::from_str(&raw).map_err(|e| malformed(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct StationProfile {
        profile_version: u64,
        station_id: String,
        broker: BrokerProfile,
        app: AppProfile,
    }

    #[test]
    fn profile_parses_all_fields() {
        let profile: StationProfile = toml::from_str(
            r#"
            profile_version = 1
            station_id = "slot-17"

            [broker]
            url = "mqtts://xxxx.eu-central-1.emqx.cloud:8883"
            username = "station-17"
            password = "s3cret"

            [app]
            name = "graz-balcony"
            lat = 47.07
            lon = 15.44
            "#,
        )
        .unwrap();
        assert_eq!(profile.profile_version, 1);
        assert_eq!(profile.station_id, "slot-17");
        assert_eq!(profile.broker.username, "station-17");
        assert_eq!(profile.app.name, "graz-balcony");
        assert_eq!(profile.app.lat, Some(47.07));
        assert_eq!(profile.app.lon, Some(15.44));
    }

    /// A hand-written profile may leave the location out entirely.
    #[test]
    fn profile_parses_without_coordinates() {
        let profile: StationProfile = toml::from_str(
            r#"
            profile_version = 1
            station_id = "slot-2"

            [broker]
            url = "mqtt://localhost:1883"
            username = "station-2"
            password = "local"

            [app]
            name = "local-test"
            "#,
        )
        .unwrap();
        assert_eq!(profile.app.lat, None);
        assert_eq!(profile.app.lon, None);
    }

    /// The provisioning service may add fields without a version bump.
    #[test]
    fn profile_ignores_unknown_fields() {
        let profile: StationProfile = toml::from_str(
            r#"
            profile_version = 1
            station_id = "slot-3"
            issued_at = "2026-07-30T10:00:00Z"

            [broker]
            url = "mqtt://localhost:1883"
            username = "station-3"
            password = "s3cret"
            keepalive = 60

            [app]
            name = "graz-balcony"
            lat = 47.07
            lon = 15.44
            elevation = 353
            "#,
        )
        .unwrap();
        assert_eq!(profile.station_id, "slot-3");
    }

    #[test]
    fn only_the_supported_profile_version_is_accepted() {
        assert!(check_profile_version(PROFILE_VERSION).is_ok());
        assert!(check_profile_version(0).is_err());
        assert!(check_profile_version(2).is_err());
    }

    #[test]
    fn slot_is_parsed_from_the_station_id() {
        assert_eq!(slot_from_station_id("slot-17").unwrap(), 17);
        assert_eq!(slot_from_station_id("slot-0").unwrap(), 0);
    }

    #[test]
    fn a_station_id_that_is_not_slot_scoped_is_rejected() {
        assert!(slot_from_station_id("graz-balcony").is_err());
        assert!(slot_from_station_id("slot-").is_err());
        assert!(slot_from_station_id("slot-abc").is_err());
        // u16, so the hub's pool cannot be addressed past its range.
        assert!(slot_from_station_id("slot-70000").is_err());
    }

    /// The version gate runs before `T`, so a profile from a mesh this station
    /// cannot honour is refused on the version — not on whatever else about it
    /// fails to deserialize.
    #[cfg(feature = "std")]
    #[test]
    fn load_profile_gates_the_version_before_the_stations_own_tables() {
        let dir = std::env::temp_dir().join("weather-station-load-profile-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("future.toml");
        // No `[broker]` and no `[app]`: `StationProfile` cannot deserialize
        // from this at all, so only an early gate produces the version error.
        std::fs::write(&path, "profile_version = 99\nstation_id = \"slot-1\"\n").unwrap();

        let err = load_profile::<StationProfile>(&path).unwrap_err();
        assert!(matches!(
            err,
            StationError::UnsupportedProfileVersion { found: 99, .. }
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// Every door reports a bad profile the same way, and names the file.
    #[cfg(feature = "std")]
    #[test]
    fn an_unreadable_profile_names_the_path() {
        let err = load_profile::<StationProfile>("/nonexistent/station.toml").unwrap_err();
        assert!(matches!(err, StationError::ProfileUnreadable { .. }));
        assert!(err.to_string().contains("/nonexistent/station.toml"));
        assert_eq!(err.kind(), crate::StationErrorKind::Profile);
    }
}
