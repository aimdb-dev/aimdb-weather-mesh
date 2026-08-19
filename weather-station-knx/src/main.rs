//! # Weather Station — KNX
//!
//! A weather mesh station whose readings come from a real KNX installation.
//! Temperature and humidity sensors on the KNX bus are read through a
//! KNXnet/IP gateway and republished into the station's mesh slot, where the
//! hub derives dew point exactly as it does for any other station.
//!
//! ```bash
//! cargo run -p weather-station-knx -- --config station.toml
//! ```
//!
//! The record graph is staged: a raw record per group address, fed by the KNX
//! connector, and a throttling transform into the mesh record that carries the
//! outbound MQTT link. That decouples the bus cadence from the mesh cadence —
//! an on-change sensor cannot flood the broker — and leaves the raw records
//! carrying exactly what the bus sent, for commissioning.
//!
//! ```text
//!   knx.temperature ──throttle──▶ station.<n>.temperature ──▶ mqtt://station/<n>/temperature
//!   knx.humidity    ──throttle──▶ station.<n>.humidity    ──▶ mqtt://station/<n>/humidity
//! ```
//!
//! `DewPointV1` is not published here: the hub derives it per slot from those
//! two records.
//!
//! The mesh records on the right of that diagram, and the handshake that earns
//! the right to publish into them, come from [`weather_station`]. This station
//! takes the advanced door — [`MeshSlot`] rather than `Station` — because its
//! readings arrive *through* the graph off the KNX connector, not from a source
//! it could hand to the facade.

mod dpt;
mod knx;

use aimdb_core::{buffer::BufferCfg, AimDbBuilder, RecordKey, StringKey};
use aimdb_knx_connector::KnxConnector;
use aimdb_tokio_adapter::{TokioAdapter, TokioRecordRegistrarExt};
use clap::Parser;
use knx::{KnxConfig, KnxProfile};
use serde::Deserialize;
use std::sync::Arc;
use tracing::info;
use weather_contracts::{HumidityV1, TemperatureV2};
use weather_station::{check_profile_version, AppProfile, BrokerProfile, MeshSlot};

/// Station-local records carrying the bus reading verbatim. They never leave
/// the process — the mesh contract is `station.<n>.*`.
const RAW_TEMPERATURE_KEY: StringKey = StringKey::new("knx.temperature");
const RAW_HUMIDITY_KEY: StringKey = StringKey::new("knx.humidity");

/// KNX weather station — republishes bus sensors into a mesh slot
#[derive(Debug, Parser)]
#[command(name = "weather-station-knx", version, about)]
struct Cli {
    /// Path to the station profile (station.toml)
    #[arg(long)]
    config: std::path::PathBuf,
}

/// The station profile: the mesh's tables plus this station's `[knx]`, so a
/// profile issued by the mesh provisioning service works unchanged once
/// `[knx]` is appended.
#[derive(Debug, Deserialize)]
struct StationProfile {
    profile_version: u64,
    station_id: String,
    broker: BrokerProfile,
    app: AppProfile,
    knx: KnxProfile,
}

#[tokio::main]
async fn main() {
    // Display-format errors (revoked slot, bad profile, …) instead of the
    // default Debug dump: these messages are what the operator reads.
    if let Err(e) = run().await {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    weather_station::init_tracing("weather_station_knx");

    let cli = Cli::parse();
    let profile: StationProfile = toml::from_str(&std::fs::read_to_string(&cli.config)?)?;
    check_profile_version(profile.profile_version)?;

    // Group addresses, datapoint types and the throttle window are validated
    // before anything is built: a typo'd address would otherwise show up as a
    // station that connects to everything and publishes nothing.
    let knx = profile.knx.resolve()?;

    // Timestamps come from the runtime rather than a clock of the station's
    // own, and the throttle is a pure function of them. Without a wall clock
    // every reading would carry timestamp 0 and the throttle would mute the
    // record after the first value, so refuse to start instead.
    check_wall_clock()?;

    // The handshake, including the pre-flight CONNECT that turns a revoked slot
    // into a startup error.
    //
    // There is deliberately no matching gateway pre-flight: the KNX connector's
    // retry loop is the right behaviour for a LAN device that reboots, and a
    // station that refused to start because the gateway was briefly down would
    // be worse than one that reconnects.
    let mesh = MeshSlot::join(&profile.station_id, &profile.app, &profile.broker).await?;

    // The KNX connector goes on before `attach`: a `link_from` is only accepted
    // once a connector claims its scheme, and the raw records below use one.
    let mut builder = mesh.attach(
        AimDbBuilder::new()
            .runtime(Arc::new(TokioAdapter::new()?))
            .with_connector(KnxConnector::new(&knx.gateway)),
    );

    register_temperature(&mut builder, &knx, mesh.temperature_key());
    register_humidity(&mut builder, &knx, mesh.humidity_key());

    log_route_table(&knx, &mesh);
    mesh.log_ready();

    // `.run()` drives both connectors, the transforms and the outbound links
    // on one task set; everything this station does is in the graph above.
    builder.run().await?;

    Ok(())
}

/// `knx.temperature` ← the bus, then the throttle into the mesh record.
///
/// The mesh record's buffer, outbound link and serializer are already on the
/// builder ([`MeshSlot::attach`]); configuring the key again adds this
/// station's feed to it.
fn register_temperature(builder: &mut AimDbBuilder, knx: &KnxConfig, mesh_key: StringKey) {
    let ga = knx.temperature.group_address.clone();
    let dpt = knx.temperature.dpt;
    let link = format!("knx://{ga}");

    builder.configure::<TemperatureV2>(RAW_TEMPERATURE_KEY, |reg| {
        // SingleLatest: a sensor has one current value. The throttle is
        // leading-edge — it drops readings inside the window rather than
        // deferring them, so a burst followed by silence leaves the mesh on
        // the burst's first value until the next telegram arrives.
        reg.buffer(BufferCfg::SingleLatest)
            .link_from(&link)
            .with_deserializer(move |ctx, data: &[u8]| {
                let celsius = dpt.decode(data).map_err(|e| reject(&ctx, &ga, data, e))?;
                ctx.log().info(&format!("🌡️  KNX {ga} → {celsius:.1}°C"));
                Ok(TemperatureV2::new(celsius, unix_millis(&ctx)))
            })
            .finish();
    });

    let min_publish_ms = knx.min_publish_ms;
    builder.configure::<TemperatureV2>(mesh_key, |reg| {
        reg.transform::<TemperatureV2, _>(RAW_TEMPERATURE_KEY, move |b| {
            b.with_state(Throttle::default()).on_value(
                move |v: &TemperatureV2, st: &mut Throttle| {
                    admit(st, v.timestamp, min_publish_ms, "temperature").then(|| v.clone())
                },
            )
        });
    });
}

/// `knx.humidity` ← the bus, then the throttle into the mesh record.
fn register_humidity(builder: &mut AimDbBuilder, knx: &KnxConfig, mesh_key: StringKey) {
    let ga = knx.humidity.group_address.clone();
    let dpt = knx.humidity.dpt;
    let link = format!("knx://{ga}");

    builder.configure::<HumidityV1>(RAW_HUMIDITY_KEY, |reg| {
        reg.buffer(BufferCfg::SingleLatest)
            .link_from(&link)
            .with_deserializer(move |ctx, data: &[u8]| {
                let percent = dpt.decode(data).map_err(|e| reject(&ctx, &ga, data, e))?;
                ctx.log().info(&format!("💧 KNX {ga} → {percent:.1}%"));
                Ok(HumidityV1 {
                    percent,
                    timestamp: unix_millis(&ctx),
                })
            })
            .finish();
    });

    let min_publish_ms = knx.min_publish_ms;
    builder.configure::<HumidityV1>(mesh_key, |reg| {
        reg.transform::<HumidityV1, _>(RAW_HUMIDITY_KEY, move |b| {
            b.with_state(Throttle::default())
                .on_value(move |v: &HumidityV1, st: &mut Throttle| {
                    admit(st, v.timestamp, min_publish_ms, "humidity").then(|| v.clone())
                })
        });
    });
}

/// Report an undecodable telegram against the group address it arrived on, and
/// return the message the deserializer rejects it with.
///
/// The value never enters the buffer. The KNX demo coerces a decode failure to
/// `0.0`; here that would publish 0 °C into the mesh and drag the hub's derived
/// dew point with it.
fn reject(ctx: &aimdb_core::RuntimeContext, ga: &str, data: &[u8], e: dpt::DecodeError) -> String {
    let msg = format!(
        "knx {ga}: undecodable telegram ({}), {} byte(s)",
        e.0,
        data.len()
    );
    ctx.log().warn(&msg);
    msg
}

// ============================================================================
// Throttle
// ============================================================================

/// At most one published value per `min_interval_ms`. The first value always
/// passes.
///
/// Publish rate is the station's own: the mesh cares that a slot publishes both
/// quantities, not how often. A bus sensor reports on change, which is the one
/// cadence a broker cannot be handed directly, so this station throttles and
/// the Open-Meteo one does not.
///
/// A pure function of the value's own timestamp — the transform keeps no clock
/// of its own, so it behaves the same on any runtime.
#[derive(Default)]
struct Throttle {
    last_published_ms: Option<u64>,
}

impl Throttle {
    fn admit(&mut self, ts_ms: u64, min_interval_ms: u64) -> bool {
        match self.last_published_ms {
            // A timestamp that moves backwards (clock step, NTP correction)
            // resets the window instead of muting the record until the clock
            // catches up.
            Some(last) if ts_ms >= last && ts_ms - last < min_interval_ms => false,
            _ => {
                self.last_published_ms = Some(ts_ms);
                true
            }
        }
    }
}

/// [`Throttle::admit`] with the suppression reported at debug, so
/// `RUST_LOG=…=debug` tells "nothing arriving from KNX" apart from "arriving
/// but suppressed".
fn admit(st: &mut Throttle, ts_ms: u64, min_interval_ms: u64, quantity: &str) -> bool {
    let admitted = st.admit(ts_ms, min_interval_ms);
    if !admitted {
        tracing::debug!(
            "throttled {quantity}: less than {}s since the last publish",
            min_interval_ms / 1_000
        );
    }
    admitted
}

// ============================================================================
// Startup reporting and checks
// ============================================================================

/// Print what the station will actually do, address by address.
///
/// The failure an operator hits in practice is a wrong group address — the
/// station starts, connects to everything, and publishes nothing. This table
/// plus the per-telegram info logs are what turn that into a five-second
/// diagnosis.
fn log_route_table(knx: &KnxConfig, mesh: &MeshSlot) {
    let temp_key = mesh.temperature_key();
    let humidity_key = mesh.humidity_key();

    // Align the record-key column so the two routes read as a table.
    let width = temp_key.as_str().len().max(humidity_key.as_str().len());

    info!("📡 KNX gateway: {}", knx.gateway);
    info!(
        "   {} (DPT {}) → {:width$} → {}",
        knx.temperature.group_address,
        knx.temperature.dpt.identifier(),
        temp_key.as_str(),
        mesh.temperature_topic(),
    );
    info!(
        "   {} (DPT {}) → {:width$} → {}",
        knx.humidity.group_address,
        knx.humidity.dpt.identifier(),
        humidity_key.as_str(),
        mesh.humidity_topic(),
    );
    if knx.min_publish_ms == 0 {
        info!("   throttle: disabled — every telegram is published");
    } else {
        info!(
            "   throttle: at most one publish per {}s per record",
            knx.min_publish_ms / 1_000
        );
    }
}

/// Refuse to start without a wall clock.
///
/// Every reading's timestamp comes from `ctx.time().unix_time()`, which is
/// `None` on a runtime with no RTC. `TokioAdapter` reads the OS clock, so this
/// only fires on a system whose clock is set before the epoch — but the cost
/// of not checking is a slot that publishes one value and then goes silent
/// forever, which reads as a hardware fault.
fn check_wall_clock() -> Result<(), String> {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|_| ())
        .map_err(|_| {
            "the system clock is before the Unix epoch, so readings would carry no \
             usable timestamp — set the clock (or NTP) before starting the station"
                .to_string()
        })
}

/// Wall-clock milliseconds from the runtime, so the station keeps no clock of
/// its own (`unix_time` is `None` on a runtime without an RTC).
fn unix_millis(ctx: &aimdb_core::RuntimeContext) -> u64 {
    ctx.time()
        .unix_time()
        .map(|(s, ns)| s.saturating_mul(1000) + (ns / 1_000_000) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"
        profile_version = 1
        station_id = "slot-17"

        [broker]
        url = "mqtts://xxxx.eu-central-1.emqx.cloud:8883"
        username = "station-17"
        password = "s3cret"

        [app]
        name = "graz-office"
        lat = 47.07
        lon = 15.44

        [knx]
        gateway = "knx://192.168.1.4:3671"
        min_publish_secs = 60

        [knx.temperature]
        group_address = "9/1/0"
        dpt = "9.001"

        [knx.humidity]
        group_address = "9/1/1"
        dpt = "9.007"
    "#;

    #[test]
    fn profile_parses_all_fields() {
        let profile: StationProfile = toml::from_str(PROFILE).unwrap();
        assert_eq!(profile.profile_version, 1);
        assert_eq!(profile.station_id, "slot-17");
        assert_eq!(profile.app.name, "graz-office");

        let knx = profile.knx.resolve().unwrap();
        assert_eq!(knx.gateway, "knx://192.168.1.4:3671");
        assert_eq!(knx.temperature.group_address, "9/1/0");
        assert_eq!(knx.humidity.group_address, "9/1/1");
        assert_eq!(knx.min_publish_ms, 60_000);
    }

    /// A mesh-issued profile deserializes unchanged once `[knx]` is appended:
    /// `app.lat` / `app.lon` are accepted and ignored, and the provisioning
    /// service may add fields without a version bump.
    #[test]
    fn profile_ignores_coordinates_and_unknown_fields() {
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
            name = "graz-office"
            lat = 47.07
            lon = 15.44
            elevation = 353

            [knx]
            gateway = "knx://192.168.1.4:3671"

            [knx.temperature]
            group_address = "9/1/0"

            [knx.humidity]
            group_address = "9/1/1"
            "#,
        )
        .unwrap();
        assert_eq!(profile.station_id, "slot-3");
        assert!(profile.knx.resolve().is_ok());
    }

    /// The `[knx]` table is what separates this station from the Open-Meteo
    /// one; a profile without it cannot be run.
    #[test]
    fn a_profile_without_the_knx_table_is_rejected() {
        let parsed: Result<StationProfile, _> = toml::from_str(
            r#"
            profile_version = 1
            station_id = "slot-3"

            [broker]
            url = "mqtt://localhost:1883"
            username = "station-3"
            password = "s3cret"

            [app]
            name = "graz-office"
            "#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn first_value_always_passes() {
        let mut t = Throttle::default();
        assert!(t.admit(1_000, 60_000));
    }

    #[test]
    fn a_value_inside_the_window_is_dropped_and_one_outside_passes() {
        let mut t = Throttle::default();
        assert!(t.admit(1_000, 60_000));
        assert!(!t.admit(30_000, 60_000));
        assert!(!t.admit(60_999, 60_000));
        assert!(t.admit(61_000, 60_000));
        // The window restarts from the value that was published, not from the
        // ones that were dropped.
        assert!(!t.admit(90_000, 60_000));
    }

    /// A clock step backwards must not mute the record until wall time catches
    /// up — it restarts the window at the new timestamp.
    #[test]
    fn a_backwards_timestamp_resets_the_window() {
        let mut t = Throttle::default();
        assert!(t.admit(100_000, 60_000));
        assert!(t.admit(50_000, 60_000));
        assert!(!t.admit(60_000, 60_000));
    }

    #[test]
    fn a_zero_interval_admits_everything() {
        let mut t = Throttle::default();
        assert!(t.admit(1_000, 0));
        assert!(t.admit(1_000, 0));
        assert!(t.admit(1_001, 0));
    }
}
