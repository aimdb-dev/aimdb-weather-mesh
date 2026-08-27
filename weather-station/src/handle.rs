//! The blocking door: a station whose caller owns the loop.

use alloc::string::{String, ToString};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use aimdb_core::{AimDbBuilder, RecordKey};
use aimdb_sync::{AimDbBuilderSyncExt, AimDbHandle, SyncProducer};
use aimdb_tokio_adapter::TokioAdapter;
use serde::Deserialize;
use weather_contracts::{HumidityV1, TemperatureV2};

use crate::{check_profile_version, AppProfile, BrokerProfile, MeshSlot, StationError};

/// How long [`StationHandle::open`] waits for the graph to start pumping.
///
/// Generous: it covers building the graph and starting both connectors, not a
/// network round-trip. Exceeding it means the runtime thread is wedged, which
/// is worth an error rather than a station that publishes into nothing.
const GRAPH_START_TIMEOUT: Duration = Duration::from_secs(10);

/// How long [`StationHandle::shutdown`] waits for the runtime thread to stop.
///
/// The same argument as [`GRAPH_START_TIMEOUT`], applied on the way out: a
/// wedged runtime thread should make shutdown fail, not hang the caller
/// forever. `AimDbHandle::detach` has no timeout of its own.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

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
    temperature: SyncProducer<TemperatureV2>,
    humidity: SyncProducer<HumidityV1>,
    /// Whether [`shutdown`](Self::shutdown) has taken the handle out of `db`.
    ///
    /// An atomic rather than a peek at the mutex below, because
    /// [`is_closed`](Self::is_closed) is what an FFI layer calls while holding
    /// its own interpreter lock. See the lock-ordering note on `shutdown`.
    closed: AtomicBool,
    /// In a mutex so [`shutdown`](Self::shutdown) can take `&self`: a
    /// `#[pymethods]` method — and the C ABI's free function after it — never
    /// receives `self` by value, and a `&mut self` door would collide with a
    /// publish already in flight.
    ///
    /// A publish never contends for this lock: [`SyncProducer`] holds its own
    /// `Weak<AimDb>` and reaches the database without going through the handle,
    /// so `shutdown` can never queue behind one.
    ///
    /// Dropped last: the producers hold a weak reference to the database this
    /// handle owns, so it has to outlive them.
    db: Mutex<Option<AimDbHandle>>,
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

        let temperature = db.producer::<TemperatureV2>(slot.temperature_key().as_str())?;
        let humidity = db.producer::<HumidityV1>(slot.humidity_key().as_str())?;

        slot.log_ready();

        Ok(Self {
            slot,
            temperature,
            humidity,
            closed: AtomicBool::new(false),
            db: Mutex::new(Some(db)),
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
    /// The blocking form parks the calling thread, which for a Python caller
    /// means parking the interpreter unless the binding releases the GIL around
    /// it. This is the alternative where that does not fit.
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
    /// Idempotent, and safe to call while another thread is publishing: this
    /// takes `&self`, so no exclusive borrow has to be won from a publish
    /// already in flight. That matters most for the shape that needs it — a
    /// signal handler closing the station while its sensor threads run.
    ///
    /// `publish_*` returns once the reading is in the buffer, not once it is on
    /// the wire, so a reading published in the last milliseconds before this
    /// call may not arrive. How often is not fixed, which is the point: over
    /// eight rounds of eight publish-then-close cycles against a loopback broker,
    /// two to five of the eight temperatures arrived and none to four of the
    /// humidities — the second of the two publishes has less time and fares
    /// worse. That is accepted rather than papered over: stations are
    /// long-lived and publish on a cadence, so the reading lost to a shutdown
    /// is one nobody would have read. A station that publishes once and exits
    /// needs a delivery signal — an ACK topic — not a close that waits, since
    /// no wait makes delivery certain.
    ///
    /// After this returns, `publish_*` fails with
    /// [`SyncError::RuntimeShutdown`](aimdb_sync::SyncError::RuntimeShutdown):
    /// dropping the handle releases the last `Arc` to the database, so the
    /// producers' weak references stop upgrading. That is deliberate. Keeping
    /// the handle alive would let `set()` go on pushing into a buffer nobody
    /// reads and go on returning `Ok`, which loses readings silently.
    ///
    /// # Lock ordering
    ///
    /// The guard is dropped *before* the runtime thread is joined — hence the
    /// `let` below rather than matching on the `take()` directly, which would
    /// extend the guard's lifetime to the end of the match. A caller that
    /// blocks on this mutex while holding a lock the runtime thread needs (an
    /// FFI layer's interpreter lock, say, when that thread logs through a
    /// bridge into it) would otherwise deadlock against its own shutdown.
    pub fn shutdown(&self) -> Result<(), StationError> {
        let taken = self
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        self.closed.store(true, Ordering::Release);
        match taken {
            Some(db) => {
                db.detach_timeout(SHUTDOWN_TIMEOUT)?;
                Ok(())
            }
            // Already shut down. Nothing to join, and nothing to report.
            None => Ok(()),
        }
    }

    /// Whether this station can still publish.
    ///
    /// True after [`shutdown`](Self::shutdown), and true whenever a publish
    /// could no longer reach the graph — the runtime thread is gone, or this
    /// process `fork`ed since the station was opened and the thread did not
    /// come across. A child inherits this struct but not the thread, so its
    /// station is closed in every sense that matters to a caller deciding
    /// whether to publish.
    ///
    /// The second half is asked of the producer rather than tracked here, so it
    /// is the very check [`publish_temperature`](Self::publish_temperature)
    /// goes through: this cannot report open while a publish would be refused.
    ///
    /// Never takes the mutex — the producer is reachable without it, which is
    /// also why a publish never queues behind a shutdown. So a caller holding
    /// an interpreter lock can ask this while a shutdown is joining the runtime
    /// thread without closing the cycle described on `shutdown`.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.temperature.check().is_err()
    }

    /// [`shutdown`](Self::shutdown) for a caller that owns the handle by value.
    ///
    /// Dropping the handle also shuts down, and reports the omission as a
    /// warning; prefer this so the shutdown is orderly.
    pub fn close(self) -> Result<(), StationError> {
        self.shutdown()
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
