//! # Weather Station
//!
//! What every weather mesh station has to agree on: the profile format, the
//! slot-scoped identity, the broker handshake, and the two records the hub
//! expects a slot to publish. A template copies what a station author changes —
//! the poll loop, the decoding, the source — not what they must not diverge on.
//! Two stations disagreeing on `slot-<n>`, the version gate or the revocation
//! policy is a mesh defect, not a template variation.
//!
//! ## Three doors onto one record graph
//!
//! - [`Station`] — the default. Join, supply one async task per quantity, run.
//! - [`MeshSlot`] — same handshake, but hands the builder back unbuilt, for a
//!   station whose readings arrive through a connector rather than a
//!   `.source()`.
//! - [`StationHandle`] (`sync` feature) — the inverse: the caller owns the
//!   loop and publishes into it. A plain thread, or Python/C/C++ through an FFI
//!   layer.
//!
//! The graph is the same in all three; only who drives it differs. That is the
//! point — a Python station and a Rust station cannot drift apart.
//!
//! Compiled examples live on [`Station`], [`MeshSlot::attach`] and
//! [`StationHandle`]; overview snippets are not doctested, because the types
//! behind them are feature-gated and a doctest cannot see the features.
//!
//! ## Runtimes
//!
//! `no_std` with `alloc`. `tokio-runtime` (default) adds the `Station` facade,
//! [`MeshSlot::attach`] and the pre-flight probe — one CONNECT before the graph
//! is built, so a revoked slot fails at startup. That is a second MQTT client
//! for one round-trip: worth it on a host, not on an MCU, which is left to the
//! connector's reconnect loop.
//!
//! An MCU station turns all of it off and keeps what the mesh defines: profile
//! tables, `slot-<n>` identity, record keys, outbound topics, and
//! [`configure_slot_records!`]. It brings its own Embassy adapter and MQTT
//! connector, which cannot be declared here — a path dependency on embassy
//! drags its whole graph into resolution and collides on
//! `links = "embassy-time"`, resolvable only by the `[patch.crates-io]` entries
//! in the *aimdb* workspace. That patch set belongs where the MCU station
//! lives, not in a manifest every host station reads.
//!
//! Both halves derive `station.<n>.temperature` and
//! `mqtt://station/<n>/temperature` from this code. A template spelling those
//! itself is the drift this crate exists to prevent.
//!
//! Rate is station freedom: a throttle belongs in the station, not here.

#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

mod broker;
#[cfg(feature = "std")]
mod clock;
mod error;
#[cfg(feature = "sync")]
mod handle;
mod profile;
mod slot;
#[cfg(feature = "tokio-runtime")]
mod station;

pub use broker::redact_url;
#[cfg(feature = "std")]
pub use clock::check_wall_clock;
pub use error::{StationError, StationErrorKind};
#[cfg(feature = "sync")]
pub use handle::StationHandle;
#[cfg(feature = "std")]
pub use profile::load_profile;
pub use profile::{
    check_profile_version, slot_from_station_id, AppProfile, BrokerProfile, PROFILE_VERSION,
};
pub use slot::{MeshSlot, MESH_BUFFER_CAPACITY};
#[cfg(feature = "tokio-runtime")]
pub use station::Station;

/// Paths for [`configure_slot_records!`] to expand against, so the macro does
/// not depend on what the calling crate happens to name its dependencies.
#[doc(hidden)]
pub mod __macro_deps {
    pub use {aimdb_core, aimdb_data_contracts, weather_contracts};
}

/// Set up tracing for a station binary.
///
/// `station_target` is the station crate's log target (its crate name with
/// underscores). Three more targets are in the fallback filter because a
/// station reports under more than its own name: `weather_station` carries the
/// handshake and the startup banner, and `aimdb` / `aimdb_core` carry
/// `ctx.log()`. Without them a misconfigured station looks exactly like a
/// healthy one — it says nothing either way.
///
/// `RUST_LOG` overrides the whole filter when set.
///
/// For a station *binary*, which is the application and therefore gets to make
/// this decision. A library must not: an FFI layer inside someone else's
/// process uses that host's logging instead — see `weather-station-py`'s
/// `init_logging`.
///
/// Does nothing if a subscriber is already installed. `try_init` rather than
/// `init` because the panic `init` raises on a second call is not a station's
/// to raise: it crosses an FFI boundary as something the host cannot catch.
#[cfg(feature = "init-tracing")]
pub fn init_tracing(station_target: &str) {
    use alloc::format;
    use tracing_subscriber::util::SubscriberInitExt;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!("{station_target}=info,weather_station=info,aimdb_core=info,aimdb=info")
                    .into()
            }),
        )
        .finish()
        .try_init();
}
