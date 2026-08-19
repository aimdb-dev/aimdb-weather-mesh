//! # Weather Station — Open-Meteo
//!
//! A weather mesh station that needs no hardware. It is configured entirely by
//! a `station.toml` profile: broker URL and credentials, assigned slot, station
//! name, and the coordinates the observations are fetched for.
//!
//! ```bash
//! cargo run -p weather-station-openmeteo -- --config station.toml
//! ```
//!
//! Publishes `TemperatureV2` and `HumidityV1` into its assigned slot
//! (`station/{slot}/…`), each fed by a source so the poll loops run as part of
//! the record graph. `DewPointV1` is not published here: the hub derives it per
//! slot from those two records.
//!
//! Everything the mesh defines — the profile format, the slot identity, the
//! broker handshake, the records and their outbound links — comes from
//! [`weather_station`]. What is left below is what a station of your own would
//! change: where the readings come from.

mod open_meteo;

use aimdb_core::{Producer, RuntimeContext};
use clap::Parser;
use open_meteo::OpenMeteoClient;
use serde::Deserialize;
use std::sync::Arc;
use tracing::info;
use weather_contracts::{HumidityV1, TemperatureV2};
use weather_station::{check_profile_version, AppProfile, BrokerProfile, Station};

/// Open-Meteo refreshes roughly every 15 minutes; polling every 5 keeps the
/// slot current without hammering a free API.
const POLL_INTERVAL_SECS: u64 = 300;

/// Open-Meteo cloud weather station — runs from a `station.toml` profile
#[derive(Debug, Parser)]
#[command(name = "weather-station-openmeteo", version, about)]
struct Cli {
    /// Path to the station profile (station.toml)
    #[arg(long)]
    config: std::path::PathBuf,
}

/// The station profile: the mesh's tables, and nothing of this station's own —
/// the coordinates it needs are already in `[app]`.
#[derive(Debug, Deserialize)]
struct StationProfile {
    profile_version: u64,
    station_id: String,
    broker: BrokerProfile,
    app: AppProfile,
}

/// Vienna — the fallback location when neither the profile nor the
/// environment names one.
const DEFAULT_LAT: f64 = 48.2082;
const DEFAULT_LON: f64 = 16.3738;

/// Where the station's coordinates came from, for the startup log.
#[derive(Debug, PartialEq)]
enum LocationSource {
    Profile,
    Environment,
    Default,
}

impl std::fmt::Display for LocationSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Profile => write!(f, "from station.toml"),
            Self::Environment => write!(f, "from WEATHER_LAT/WEATHER_LON"),
            Self::Default => write!(f, "default: Vienna"),
        }
    }
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
    weather_station::init_tracing("weather_station_openmeteo");

    let cli = Cli::parse();
    let profile: StationProfile = toml::from_str(&std::fs::read_to_string(&cli.config)?)?;
    check_profile_version(profile.profile_version)?;

    let (lat, lon, location_source) = resolve_location(
        (profile.app.lat, profile.app.lon),
        (env_coord("WEATHER_LAT")?, env_coord("WEATHER_LON")?),
    )?;

    // One client behind both records: the two sources poll on the same cadence
    // and share the observation, so a cycle costs one HTTP request and both
    // records carry the same timestamp.
    let client = Arc::new(OpenMeteoClient::new(lat, lon));
    let temp_client = Arc::clone(&client);

    let station = Station::join(&profile.station_id, &profile.app, &profile.broker).await?;
    info!("🌍 Weather location: {lat:.2}°N, {lon:.2}°E ({location_source})");

    station
        .temperature(move |ctx, producer| temperature_source(ctx, producer, temp_client))
        .humidity(move |ctx, producer| humidity_source(ctx, producer, client))
        .run()
        .await?;

    Ok(())
}

/// Feeds `station.{slot}.temperature`.
///
/// Replacing the `client.current` call with a sensor read leaves everything
/// above unchanged — this is the whole of what a station template supplies.
async fn temperature_source(
    ctx: RuntimeContext,
    producer: Producer<TemperatureV2>,
    client: Arc<OpenMeteoClient>,
) {
    loop {
        match client.current(&ctx).await {
            Ok(obs) => {
                producer.produce(TemperatureV2::new(obs.temperature as f32, obs.timestamp));
                ctx.log()
                    .info(&format!("🌡️  Published {:.1}°C", obs.temperature));
            }
            Err(e) => ctx.log().warn(&format!("Open-Meteo fetch failed: {e}")),
        }

        ctx.time().sleep_secs(POLL_INTERVAL_SECS).await;
    }
}

/// Feeds `station.{slot}.humidity` from the same observation the temperature
/// source used (see [`OpenMeteoClient::current`]).
async fn humidity_source(
    ctx: RuntimeContext,
    producer: Producer<HumidityV1>,
    client: Arc<OpenMeteoClient>,
) {
    loop {
        match client.current(&ctx).await {
            Ok(obs) => {
                producer.produce(HumidityV1 {
                    percent: obs.humidity as f32,
                    timestamp: obs.timestamp,
                });
                ctx.log()
                    .info(&format!("💧 Published {:.1}%", obs.humidity));
            }
            Err(e) => ctx.log().warn(&format!("Open-Meteo fetch failed: {e}")),
        }

        ctx.time().sleep_secs(POLL_INTERVAL_SECS).await;
    }
}

/// Pick the coordinates the weather data is fetched for.
///
/// The profile wins when it carries them: a joined station reports from the
/// coarsened location the mesh published for it, so an environment variable
/// cannot move it on the public map. `WEATHER_LAT`/`WEATHER_LON` fill in for a
/// hand-written profile that omits them, and Vienna is the last resort.
///
/// Coordinates are taken as a pair from one source; half a location is an error
/// rather than a silent mix of two sources.
fn resolve_location(
    profile: (Option<f64>, Option<f64>),
    env: (Option<f64>, Option<f64>),
) -> Result<(f64, f64, LocationSource), String> {
    match profile {
        (Some(lat), Some(lon)) => return Ok((lat, lon, LocationSource::Profile)),
        (None, None) => {}
        _ => {
            return Err(
                "station.toml sets only one of app.lat / app.lon — give both or neither".into(),
            )
        }
    }

    match env {
        (Some(lat), Some(lon)) => Ok((lat, lon, LocationSource::Environment)),
        (None, None) => Ok((DEFAULT_LAT, DEFAULT_LON, LocationSource::Default)),
        _ => Err("set WEATHER_LAT and WEATHER_LON together, or neither".into()),
    }
}

/// Read one coordinate from the environment, rejecting a value that is set but
/// unparseable instead of silently falling back.
fn env_coord(var: &str) -> Result<Option<f64>, String> {
    match std::env::var(var) {
        Ok(raw) => raw
            .parse()
            .map(Some)
            .map_err(|_| format!("{var}='{raw}' is not a number")),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mesh tables come from `weather-station` and are tested there; what
    /// this station adds is reading its location out of `[app]`.
    #[test]
    fn profile_parses_the_stations_own_fields() {
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
        assert_eq!(profile.app.name, "graz-balcony");
        assert_eq!(profile.app.lat, Some(47.07));
        assert_eq!(profile.app.lon, Some(15.44));
    }

    #[test]
    fn location_prefers_the_profile_over_the_environment() {
        assert_eq!(
            resolve_location((Some(47.07), Some(15.44)), (Some(1.0), Some(2.0))).unwrap(),
            (47.07, 15.44, LocationSource::Profile)
        );
    }

    #[test]
    fn location_falls_back_to_environment_then_vienna() {
        assert_eq!(
            resolve_location((None, None), (Some(1.0), Some(2.0))).unwrap(),
            (1.0, 2.0, LocationSource::Environment)
        );
        assert_eq!(
            resolve_location((None, None), (None, None)).unwrap(),
            (DEFAULT_LAT, DEFAULT_LON, LocationSource::Default)
        );
    }

    #[test]
    fn location_rejects_half_a_pair() {
        assert!(resolve_location((Some(47.07), None), (None, None)).is_err());
        assert!(resolve_location((None, None), (None, Some(15.44))).is_err());
    }
}
