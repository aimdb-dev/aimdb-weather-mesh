//! # Weather Station
//!
//! What every weather mesh station has to agree on: the profile format, the
//! slot-scoped identity, the broker handshake, and the two records the hub
//! expects a slot to publish.
//!
//! A station template copies the parts a station author reads and changes — the
//! poll loop, the bus decoding, the source implementation. It does not copy the
//! parts a station author must not diverge on: two stations disagreeing on the
//! `slot-<n>` format, the profile version gate or the revocation policy is a
//! mesh defect, not a template variation. Those live here, versioned.
//!
//! ## Two doors
//!
//! [`Station`] is the default. Join, supply one async task per quantity, run:
//!
//! ```no_run
//! # use aimdb_core::{Producer, RuntimeContext};
//! # use weather_contracts::{Humidity, Temperature};
//! # use weather_station::{AppProfile, BrokerProfile, Station, StationError};
//! # async fn temperature_source(ctx: RuntimeContext, producer: Producer<Temperature>) {}
//! # async fn humidity_source(ctx: RuntimeContext, producer: Producer<Humidity>) {}
//! # async fn example(app: &AppProfile, broker: &BrokerProfile) -> Result<(), StationError> {
//! Station::join("slot-17", app, broker)
//!     .await?
//!     .temperature(temperature_source)
//!     .humidity(humidity_source)
//!     .run()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! [`MeshSlot`] is for stations that ingest *through* the record graph rather
//! than from a `.source()` — readings arriving over a connector AimDB already
//! speaks, optionally through records of the station's own. It performs the same
//! handshake and attaches the same mesh records, but hands the builder back
//! unbuilt so the station can add its intake.
//!
//! ## The blocking door
//!
//! Both doors above are async: the graph owns the loop and the station hands it
//! a task. A caller that owns its own loop — a plain thread, or a Python, C or
//! C++ station reaching Rust through an FFI layer — wants the inverse, and gets
//! [`StationHandle`] behind the `sync` feature:
//!
//! ```text
//! let station = StationHandle::open_profile("station.toml")?;
//! station.publish_temperature(read_sensor())?;
//! ```
//!
//! (Shown rather than compiled: the type is absent without the feature.
//! [`StationHandle`]'s own example is the compiled one.)
//!
//! The record graph is the same one the async doors build; only who drives it
//! differs. That is the point — a Python station and a Rust station join the
//! mesh through one implementation, so neither can drift from the other.
//!
//! ## What stays in the template
//!
//! Anything above the mesh contract: the source implementation, the profile's
//! own tables, and publish cadence. A station is free to publish as fast as its
//! sensor reports or to throttle to whatever suits it — rate is station
//! freedom, so a throttle belongs in the station, not behind this boundary.

mod broker;
mod error;
#[cfg(feature = "sync")]
mod handle;
mod profile;
mod slot;
mod station;

pub use broker::redact_url;
pub use error::StationError;
#[cfg(feature = "sync")]
pub use handle::StationHandle;
pub use profile::{
    check_profile_version, slot_from_station_id, AppProfile, BrokerProfile, PROFILE_VERSION,
};
pub use slot::MeshSlot;
pub use station::Station;

/// Set up tracing for a station binary.
///
/// `station_target` is the station crate's log target (its crate name with
/// underscores). Three more targets are in the fallback filter because the
/// station's own name no longer covers what it reports: `weather_station`
/// carries the handshake and the startup banner, and `aimdb` / `aimdb_core`
/// carry `ctx.log()`. Without them a misconfigured station looks exactly like a
/// healthy one — it says nothing either way.
///
/// `RUST_LOG` overrides the whole filter when set.
pub fn init_tracing(station_target: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!("{station_target}=info,weather_station=info,aimdb_core=info,aimdb=info")
                    .into()
            }),
        )
        .init();
}
