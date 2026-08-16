//! The blocking door: a station whose caller owns the loop.

use alloc::string::{String, ToString};
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aimdb_core::{AimDbBuilder, RecordKey};
use aimdb_sync::{AimDbBuilderSyncExt, AimDbHandle, SyncProducer};
use aimdb_tokio_adapter::TokioAdapter;
use serde::Deserialize;
use weather_contracts::{Humidity, Temperature};

use crate::{check_profile_version, AppProfile, BrokerProfile, MeshSlot, StationError};

/// How long [`StationHandle::open`] waits for the graph to start pumping.
///
/// Generous: it covers building the graph and starting both connectors, not a
/// network round-trip. Exceeding it means the runtime thread is wedged, which
/// is worth an error rather than a station that publishes into nothing.
const GRAPH_START_TIMEOUT: Duration = Duration::from_secs(10);

/// A joined mesh station driven from outside the async runtime.
///
/// The record graph runs on a background thread; this is the handle onto it.
/// Nothing about the mesh — the profile gate, the slot identity, the handshake,
/// the outbound links — is the caller's to get right, and no async appears in
/// the API, which is what makes this the type an FFI layer binds.
///
/// ```no_run
/// # use std::thread;
/// # use std::time::Duration;
/// # use weather_station::{StationHandle, StationError};
/// # fn read_sensor() -> (f32, f32) { (21.5, 55.0) }
/// # fn example() -> Result<(), StationError> {
/// let station = StationHandle::open_profile("station.toml")?;
///
/// loop {
///     let (celsius, percent) = read_sensor();
///     station.publish_temperature(celsius)?;
///     station.publish_humidity(percent)?;
///     thread::sleep(Duration::from_secs(60));
/// }
/// # }
/// ```
///
/// The Rust templates use [`Station`](crate::Station) or
/// [`MeshSlot`](crate::MeshSlot) instead: they are already inside a Tokio
/// runtime, and this type would make them block a worker thread on a call the
/// graph could drive itself.
///
/// # Not reentrant into a runtime
///
/// [`open`](Self::open) blocks on the broker pre-flight, so it must not be
/// called from inside a Tokio runtime. That suits every FFI caller — a Python
/// or C station owns a plain OS thread — and it is why the async doors exist
/// for everyone else.
pub struct StationHandle {
    slot: MeshSlot,
    temperature: SyncProducer<Temperature>,
    humidity: SyncProducer<Humidity>,
    /// Dropped last: the producers hold a weak reference to the database this
    /// handle owns, so it has to outlive them.
    db: AimDbHandle,
}

/// The mesh tables, parsed on behalf of a caller that has a file rather than a
/// struct of its own.
///
/// The Rust doors take the tables already parsed, because a station composes
/// them into a profile naming its own extras. An FFI caller has no such struct,
/// so this door owns the parse — which is also the only way the profile gate
/// stays on the mesh's side of the boundary.
#[derive(Debug, Deserialize)]
struct MeshProfile {
    profile_version: u64,
    station_id: String,
    broker: BrokerProfile,
    app: AppProfile,
}

impl StationHandle {
    /// Join the mesh from a `station.toml` path.
    ///
    /// Reads the mesh tables, checks the profile version, performs the
    /// handshake and starts the graph. Tables the mesh does not define are
    /// ignored, so a profile carrying a station's own extras still opens.
    pub fn open_profile(path: impl AsRef<Path>) -> Result<Self, StationError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|e| StationError::ProfileUnreadable {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let profile: MeshProfile =
            toml::from_str(&raw).map_err(|e| StationError::ProfileMalformed {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;

        check_profile_version(profile.profile_version)?;
        Self::open(&profile.station_id, &profile.app, &profile.broker)
    }

    /// Join the mesh from tables the caller has already parsed.
    ///
    /// The path [`open_profile`](Self::open_profile) takes once it has read the
    /// file; use it directly when the profile arrives some other way.
    pub fn open(
        station_id: &str,
        app: &AppProfile,
        broker: &BrokerProfile,
    ) -> Result<Self, StationError> {
        // The pre-flight is async and this door is not, so it gets a runtime of
        // its own for the duration of the handshake. The graph's runtime is a
        // separate thread that `attach()` owns.
        let preflight = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| StationError::BrokerUnreachable {
                url: crate::redact_url(&broker.url),
                reason: e.to_string(),
            })?;
        let slot = preflight.block_on(MeshSlot::join(station_id, app, broker))?;
        drop(preflight);

        let mut builder = slot.attach(AimDbBuilder::new().runtime(Arc::new(TokioAdapter::new()?)));

        // Registered after the slot's records, so it is the last future the
        // runner collects and therefore the last one polled in the first pass:
        // when it fires, both outbound links have subscribed to their buffers.
        // Without this gate the first reading is lost roughly seven times in
        // eight — `set()` still returns `Ok`, because a broadcast buffer
        // accepts a value nobody is reading yet.
        let (started_tx, started_rx) = mpsc::sync_channel::<()>(1);
        builder.on_start(move |_ctx| async move {
            let _ = started_tx.send(());
        });

        let db = builder.attach()?;
        started_rx
            .recv_timeout(GRAPH_START_TIMEOUT)
            .map_err(|_| StationError::GraphStartTimeout(GRAPH_START_TIMEOUT))?;

        let temperature = db.producer::<Temperature>(slot.temperature_key().as_str())?;
        let humidity = db.producer::<Humidity>(slot.humidity_key().as_str())?;

        slot.log_ready();

        Ok(Self {
            slot,
            temperature,
            humidity,
            db,
        })
    }

    /// Publish a temperature reading, blocking until the graph accepts it.
    ///
    /// The reading is stamped with the current wall-clock time. The hub pairs
    /// whatever it last saw of each quantity, so temperature and humidity do
    /// not have to be published together or share a timestamp.
    pub fn publish_temperature(&self, celsius: f32) -> Result<(), StationError> {
        self.temperature
            .set(Temperature::new(celsius, unix_millis()?))?;
        Ok(())
    }

    /// Publish a humidity reading, blocking until the graph accepts it.
    pub fn publish_humidity(&self, percent: f32) -> Result<(), StationError> {
        self.humidity.set(Humidity {
            percent,
            timestamp: unix_millis()?,
        })?;
        Ok(())
    }

    /// [`publish_temperature`](Self::publish_temperature) without blocking:
    /// fails rather than waiting when the outbound buffer is full.
    ///
    /// The blocking form parks the calling thread, which for a Python caller
    /// means parking the interpreter unless the binding releases the GIL around
    /// it. This is the alternative where that does not fit.
    pub fn try_publish_temperature(&self, celsius: f32) -> Result<(), StationError> {
        self.temperature
            .try_set(Temperature::new(celsius, unix_millis()?))?;
        Ok(())
    }

    /// [`publish_humidity`](Self::publish_humidity) without blocking.
    pub fn try_publish_humidity(&self, percent: f32) -> Result<(), StationError> {
        self.humidity.try_set(Humidity {
            percent,
            timestamp: unix_millis()?,
        })?;
        Ok(())
    }

    /// This station's place in the mesh — slot number, record keys, topics.
    pub fn mesh_slot(&self) -> &MeshSlot {
        &self.slot
    }

    /// Stop the station and shut the runtime thread down.
    ///
    /// Dropping the handle without this also shuts down, but reports the
    /// omission as a warning; an FFI layer should call this from its free
    /// function so the shutdown is orderly.
    pub fn close(self) -> Result<(), StationError> {
        self.db.detach()?;
        Ok(())
    }
}

/// Wall-clock milliseconds.
///
/// A reading with no usable timestamp is worse than no reading: the hub keys
/// its dew-point join off them, so a station whose clock is unset would poison
/// its slot rather than merely go quiet.
fn unix_millis() -> Result<u64, StationError> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| StationError::NoWallClock)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FFI door parses the mesh tables itself, and ignores a station's own.
    #[test]
    fn the_mesh_tables_parse_out_of_a_station_specific_profile() {
        let profile: MeshProfile = toml::from_str(
            r#"
            profile_version = 1
            station_id = "slot-17"

            [broker]
            url = "mqtts://broker.example.com:8883"
            username = "station-17"
            password = "s3cret"

            [app]
            name = "graz-office"
            lat = 47.07
            lon = 15.44

            [knx]
            gateway = "knx://192.168.1.4:3671"
            "#,
        )
        .unwrap();
        assert_eq!(profile.station_id, "slot-17");
        assert_eq!(profile.app.name, "graz-office");
        assert_eq!(profile.broker.username, "station-17");
    }

    #[test]
    fn a_missing_profile_names_the_path() {
        let Err(err) = StationHandle::open_profile("/nonexistent/station.toml") else {
            panic!("a missing profile must not open a station");
        };
        assert!(err.to_string().contains("/nonexistent/station.toml"));
    }

    #[test]
    fn a_profile_from_a_future_mesh_is_refused_before_any_connection() {
        let dir = std::env::temp_dir().join("weather-station-handle-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("station.toml");
        std::fs::write(
            &path,
            r#"
            profile_version = 99
            station_id = "slot-1"

            [broker]
            url = "mqtt://127.0.0.1:1"
            username = "u"
            password = "p"

            [app]
            name = "future"
            "#,
        )
        .unwrap();

        // Rejected on the version, not by failing to reach 127.0.0.1:1.
        let Err(err) = StationHandle::open_profile(&path) else {
            panic!("a profile_version this station cannot honour must not open");
        };
        assert!(matches!(
            err,
            StationError::UnsupportedProfileVersion { found: 99, .. }
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn readings_carry_a_wall_clock_timestamp() {
        let now = unix_millis().unwrap();
        // Sanity: milliseconds since the epoch, not seconds and not zero.
        assert!(now > 1_700_000_000_000);
    }
}
