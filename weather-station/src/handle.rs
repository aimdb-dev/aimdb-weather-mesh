//! The blocking door: a station whose caller owns the loop.

use alloc::string::{String, ToString};
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use aimdb_core::{AimDbBuilder, RecordKey};
use aimdb_sync::{AimDbBuilderSyncExt, AimDbHandle, SyncProducer};
use aimdb_tokio_adapter::TokioAdapter;
use serde::Deserialize;
use weather_contracts::{HumidityV1, TemperatureV2};

use crate::clock::unix_millis;
use crate::{load_profile, AppProfile, BrokerProfile, MeshSlot, StationError};

/// How long [`StationHandle::open`] waits for the graph to start pumping.
///
/// Generous: it covers building the graph and starting both connectors, not a
/// network round-trip. Exceeding it means the runtime thread is wedged.
const GRAPH_START_TIMEOUT: Duration = Duration::from_secs(10);

/// How long [`StationHandle::shutdown`] waits for the runtime thread to stop.
///
/// `AimDbHandle::detach` has no timeout of its own, and a wedged runtime thread
/// should fail the shutdown rather than hang the caller.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// A joined mesh station driven from outside the async runtime.
///
/// The record graph runs on a background thread; this is the handle onto it.
/// No async in the API, which is what makes it the type an FFI layer binds.
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
/// Not reentrant into a runtime: [`open`](Self::open) blocks on the broker
/// pre-flight. Callers already inside Tokio want [`Station`](crate::Station) or
/// [`MeshSlot`](crate::MeshSlot); an FFI caller owns a plain OS thread anyway.
pub struct StationHandle {
    slot: MeshSlot,
    temperature: SyncProducer<TemperatureV2>,
    humidity: SyncProducer<HumidityV1>,
    // Last, so it drops after the producers: their `Weak<AimDb>` points at what
    // it owns.
    db: AimDbHandle,
}

/// The mesh tables, parsed on behalf of a caller that has a file rather than a
/// struct of its own.
///
/// The Rust doors take the tables already parsed. An FFI caller has no such
/// struct, so this door owns the parse — which keeps the profile gate on the
/// mesh's side of the boundary.
#[derive(Debug, Deserialize)]
struct MeshProfile {
    station_id: String,
    broker: BrokerProfile,
    app: AppProfile,
}

impl StationHandle {
    /// Join the mesh from a `station.toml` path.
    ///
    /// Reads the mesh tables through [`load_profile`], performs the handshake
    /// and starts the graph. Tables the mesh does not define are ignored, so a
    /// profile carrying a station's own extras still opens.
    pub fn open_profile(path: impl AsRef<Path>) -> Result<Self, StationError> {
        let profile: MeshProfile = load_profile(path)?;
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
        // Without this gate the first reading is usually lost — `set()` still
        // returns `Ok`, because a broadcast buffer accepts a value nobody is
        // reading yet.
        let (started_tx, started_rx) = mpsc::sync_channel::<()>(1);
        builder.on_start(move |_ctx| async move {
            let _ = started_tx.send(());
        });

        let db = builder.attach()?;
        started_rx
            .recv_timeout(GRAPH_START_TIMEOUT)
            .map_err(|_| StationError::GraphStartTimeout(GRAPH_START_TIMEOUT))?;

        let temperature = db.producer::<TemperatureV2>(slot.temperature_key().as_str())?;
        let humidity = db.producer::<HumidityV1>(slot.humidity_key().as_str())?;

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
            .set(TemperatureV2::new(celsius, unix_millis()?))?;
        Ok(())
    }

    /// Publish a humidity reading, blocking until the graph accepts it.
    pub fn publish_humidity(&self, percent: f32) -> Result<(), StationError> {
        self.humidity.set(HumidityV1 {
            percent,
            timestamp: unix_millis()?,
        })?;
        Ok(())
    }

    /// [`publish_temperature`](Self::publish_temperature) without blocking:
    /// fails rather than waiting when the outbound buffer is full.
    ///
    /// The blocking form parks the calling thread — for a Python caller, the
    /// interpreter, unless the binding releases the GIL around it.
    pub fn try_publish_temperature(&self, celsius: f32) -> Result<(), StationError> {
        self.temperature
            .try_set(TemperatureV2::new(celsius, unix_millis()?))?;
        Ok(())
    }

    /// [`publish_humidity`](Self::publish_humidity) without blocking.
    pub fn try_publish_humidity(&self, percent: f32) -> Result<(), StationError> {
        self.humidity.try_set(HumidityV1 {
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
    /// Idempotent, and safe to call while another thread is publishing: it
    /// takes `&self`, so no exclusive borrow has to be won from a publish
    /// already in flight — the shape a signal handler needs.
    ///
    /// `publish_*` returns once the reading is in the buffer, not once it is on
    /// the wire, so a reading published in the last milliseconds before this
    /// call may not arrive. That is accepted rather than papered over: stations
    /// publish on a cadence, and no wait makes delivery certain. A station that
    /// publishes once and exits needs a delivery signal — an ACK topic.
    ///
    /// After this returns, `publish_*` fails with
    /// [`SyncError::RuntimeShutdown`](aimdb_sync::SyncError::RuntimeShutdown),
    /// deliberately: keeping the database alive would let `set()` go on
    /// returning `Ok` into a buffer nobody reads.
    pub fn shutdown(&self) -> Result<(), StationError> {
        self.db.shutdown_timeout(SHUTDOWN_TIMEOUT)?;
        Ok(())
    }

    /// Whether this station can still publish.
    ///
    /// True after [`shutdown`](Self::shutdown), and true whenever a publish
    /// could no longer reach the graph — the runtime thread is gone, or this
    /// process `fork`ed since the station was opened and the thread did not
    /// come across.
    ///
    /// The second half is asked of the producer rather than tracked here, so it
    /// is the very check a publish goes through: this cannot report open while
    /// a publish would be refused.
    ///
    /// Neither half takes a lock, so a caller holding an interpreter lock can
    /// ask this while another thread is joining the runtime thread.
    pub fn is_closed(&self) -> bool {
        self.db.is_closed() || self.temperature.check().is_err()
    }

    /// [`shutdown`](Self::shutdown) for a caller that owns the handle by value.
    ///
    /// Dropping the handle also shuts down, and reports the omission as a
    /// warning; prefer this so the shutdown is orderly.
    pub fn close(self) -> Result<(), StationError> {
        self.shutdown()
    }
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
        // The coordinates cross the FFI boundary as `ws_station_lat`/`_lon`, so
        // this parse is the only one: a station on a foreign runtime reads them
        // back rather than scanning the file again.
        assert_eq!(profile.app.lat, Some(47.07));
        assert_eq!(profile.app.lon, Some(15.44));
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
}
