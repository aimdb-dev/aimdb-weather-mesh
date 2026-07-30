//! # Weather Station — Open-Meteo
//!
//! The no-hardware station template for the public weather mesh (design 042
//! §7/§8): configured entirely by the `station.toml` profile that `aimdb join`
//! writes — broker URL and credentials, assigned slot, station name, and the
//! (coarsened) location the real weather data is fetched for. One profile,
//! one command:
//!
//! ```bash
//! aimdb join https://mesh.aimdb.dev                             # writes station.toml
//! cargo run -p weather-station-openmeteo -- --config station.toml
//! ```
//!
//! Publishes `Temperature` and `Humidity` into its assigned slot
//! (`station/{slot}/…`), each fed by a `.source()` so the poll loops are part
//! of the record graph the runner drives. `DewPoint` is deliberately absent:
//! the hub derives it per slot from those two records, so a station publishing
//! it would be producing traffic nothing subscribes to.

mod open_meteo;

use aimdb_core::{buffer::BufferCfg, AimDbBuilder, Producer, RuntimeContext, StringKey};
use aimdb_data_contracts::Linkable;
use aimdb_mqtt_connector::MqttConnector;
use aimdb_tokio_adapter::{TokioAdapter, TokioRecordRegistrarExt};
use clap::Parser;
use open_meteo::OpenMeteoClient;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;
use weather_contracts::{Humidity, Temperature};

/// Open-Meteo refreshes roughly every 15 minutes; polling every 5 keeps the
/// slot current without hammering a free API.
const POLL_INTERVAL_SECS: u64 = 300;

/// Open-Meteo cloud weather station — runs from a `station.toml` profile
#[derive(Debug, Parser)]
#[command(name = "weather-station-openmeteo", version, about)]
struct Cli {
    /// Path to the station profile written by `aimdb join` (design 043 §4)
    #[arg(long)]
    config: std::path::PathBuf,
}

/// The station profile (design 043 §4). Unknown fields are ignored so the
/// service can extend the format without a version bump.
#[derive(Debug, Deserialize)]
struct StationProfile {
    profile_version: u64,
    station_id: String,
    broker: BrokerProfile,
    app: AppProfile,
}

#[derive(Debug, Deserialize)]
struct BrokerProfile {
    url: String,
    username: String,
    password: String,
}

/// The flagship's `app` map: name + service-coarsened coordinates (042 D8).
///
/// The coordinates are optional so a hand-written profile can leave the
/// location to `WEATHER_LAT`/`WEATHER_LON` — see [`resolve_location`].
#[derive(Debug, Deserialize)]
struct AppProfile {
    name: String,
    lat: Option<f64>,
    lon: Option<f64>,
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
    // default Debug dump — these messages are the failure UX (design 042 §9).
    if let Err(e) = run().await {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // `aimdb` is the runtime-context log target (`ctx.log()` via the adapter) —
    // without it in the fallback filter, everything the sources report is
    // invisible.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "weather_station_openmeteo=info,aimdb_core=info,aimdb=info".into()
            }),
        )
        .init();

    let cli = Cli::parse();
    let profile: StationProfile = toml::from_str(&std::fs::read_to_string(&cli.config)?)?;

    if profile.profile_version != 1 {
        return Err(format!(
            "unsupported profile_version {} (this station understands 1) — \
             re-run `aimdb join` with a current CLI",
            profile.profile_version
        )
        .into());
    }

    // The flagship assigns slot-scoped identities: station_id = "slot-<n>"
    // maps onto the hub's pool records station.<n>.* (design 042 §4).
    let slot: u16 = profile
        .station_id
        .strip_prefix("slot-")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| {
            format!(
                "station_id '{}' is not of the form slot-<n> — \
                 is this profile from a weather-mesh deployment?",
                profile.station_id
            )
        })?;

    let (lat, lon, location_source) = resolve_location(
        (profile.app.lat, profile.app.lon),
        (env_coord("WEATHER_LAT")?, env_coord("WEATHER_LON")?),
    )?;

    info!("🚀 Starting Weather Station \"{}\"", profile.app.name);
    info!(
        "📡 Broker: {} (slot {})",
        redact_url(&profile.broker.url),
        slot
    );
    info!("🌍 Weather location: {lat:.2}°N, {lon:.2}°E ({location_source})");

    let client_id = format!("weather-station-{slot}");

    // Fail fast on a dead credential instead of retrying forever: a revoked
    // slot should say so at the moment the user is looking (design 042 §9).
    preflight_broker_check(&profile.broker, &client_id).await?;

    let mqtt_url = url_with_credentials(&profile.broker)?;

    let adapter = Arc::new(TokioAdapter::new()?);

    let mut builder = AimDbBuilder::new()
        .runtime(adapter)
        .with_connector(MqttConnector::new(&mqtt_url).with_client_id(&client_id));

    // One client behind both records: the two sources poll on the same cadence
    // and share the observation, so a cycle costs one HTTP request and both
    // records carry one timestamp.
    let client = Arc::new(OpenMeteoClient::new(lat, lon));

    let temp_key = StringKey::intern(format!("station.{slot}.temperature"));
    let temp_topic = format!("mqtt://station/{slot}/temperature");
    let temp_client = Arc::clone(&client);
    builder.configure::<Temperature>(temp_key, |reg| {
        reg.buffer(BufferCfg::SpmcRing { capacity: 10 });
        reg.source(move |ctx, producer| temperature_source(ctx, producer, temp_client));
        reg.link_to(&temp_topic)
            .with_serializer(|_ctx, t: &Temperature| {
                t.to_bytes()
                    .map_err(|_| aimdb_core::connector::SerializeError::InvalidData)
            })
            .finish();
    });

    let humidity_key = StringKey::intern(format!("station.{slot}.humidity"));
    let humidity_topic = format!("mqtt://station/{slot}/humidity");
    builder.configure::<Humidity>(humidity_key, |reg| {
        reg.buffer(BufferCfg::SpmcRing { capacity: 10 });
        reg.source(move |ctx, producer| humidity_source(ctx, producer, client));
        reg.link_to(&humidity_topic)
            .with_serializer(|_ctx, h: &Humidity| {
                h.to_bytes()
                    .map_err(|_| aimdb_core::connector::SerializeError::InvalidData)
            })
            .finish();
    });

    info!("");
    info!("🎯 Weather Station \"{}\" ready!", profile.app.name);
    info!("📡 Publishing to MQTT topics:");
    info!("   - {}", temp_topic);
    info!("   - {}", humidity_topic);
    info!("   (dew point is derived at the hub from these two)");
    info!("");
    info!("Press Ctrl+C to stop");

    // `.run()` drives the sources, the outbound links and the connector on one
    // task set — everything this station does is in the graph above.
    builder.run().await?;

    Ok(())
}

/// Feeds `station.{slot}.temperature`.
///
/// This is the seam the template is built around: swap the `client.current`
/// call for a sensor read and the record wiring above is unchanged.
async fn temperature_source(
    ctx: RuntimeContext,
    producer: Producer<Temperature>,
    client: Arc<OpenMeteoClient>,
) {
    loop {
        match client.current(&ctx).await {
            Ok(obs) => {
                producer.produce(Temperature::new(obs.temperature as f32, obs.timestamp));
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
    producer: Producer<Humidity>,
    client: Arc<OpenMeteoClient>,
) {
    loop {
        match client.current(&ctx).await {
            Ok(obs) => {
                producer.produce(Humidity {
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

/// One CONNECT → CONNACK round-trip before building the database.
///
/// The connector's event loop retries connection errors forever (by design —
/// brokers come and go). For an admission-scoped credential that is the wrong
/// UX: a revoked slot would retry silently for eternity. Probe once and turn
/// an auth rejection into a human answer (design 042 §9).
async fn preflight_broker_check(
    broker: &BrokerProfile,
    client_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let (tls, host, port) = split_broker_url(&broker.url)?;

    let mut opts = rumqttc::MqttOptions::new(format!("{client_id}-preflight"), host, port);
    opts.set_keep_alive(Duration::from_secs(10));
    opts.set_credentials(&broker.username, &broker.password);
    if tls {
        opts.set_transport(rumqttc::Transport::Tls(rumqttc::TlsConfiguration::Native));
    }

    let (client, mut event_loop) = rumqttc::AsyncClient::new(opts, 4);
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match event_loop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => return Ok(()),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }
    })
    .await;
    let _ = client.disconnect().await;

    match result {
        Ok(Ok(())) => {
            info!("✅ Broker accepted the station credential");
            Ok(())
        }
        Ok(Err(rumqttc::ConnectionError::ConnectionRefused(code))) => Err(format!(
            "the broker rejected this station's credential ({code:?}).\n  \
             The slot was likely revoked (silent for 30 days, or by the operator).\n  \
             Re-join the mesh for a fresh slot: aimdb join <provisioning-url>"
        )
        .into()),
        Ok(Err(e)) => Err(format!(
            "cannot reach the broker at {}: {e}",
            redact_url(&broker.url)
        )
        .into()),
        Err(_) => Err(format!(
            "timed out connecting to the broker at {}",
            redact_url(&broker.url)
        )
        .into()),
    }
}

/// Pick the coordinates the weather data is fetched for.
///
/// The profile wins when it carries them: 043 §3.1 makes the service's
/// coarsened `app.lat`/`app.lon` the values a joined station must report from,
/// so an environment variable does not get to move a station on the public map.
/// `WEATHER_LAT`/`WEATHER_LON` fill in for a hand-written profile that omits
/// them, and Vienna is the last resort.
///
/// Coordinates are taken as a pair — half a location is a mistake worth saying
/// out loud rather than silently mixing two sources.
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

/// Split a `mqtt[s]://host[:port]` broker URL into (tls, host, port).
fn split_broker_url(url: &str) -> Result<(bool, String, u16), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("broker URL '{url}' has no scheme"))?;
    let tls = match scheme {
        "mqtt" => false,
        "mqtts" => true,
        other => return Err(format!("unsupported broker scheme '{other}'")),
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse()
                .map_err(|_| format!("invalid broker port in '{url}'"))?,
        ),
        None => (authority.to_string(), if tls { 8883 } else { 1883 }),
    };
    Ok((tls, host, port))
}

/// Embed the profile's credential into the connector URL
/// (`mqtts://user:pass@host:port`), the form the MQTT connector parses.
fn url_with_credentials(broker: &BrokerProfile) -> Result<String, String> {
    let (scheme, rest) = broker
        .url
        .split_once("://")
        .ok_or_else(|| format!("broker URL '{}' has no scheme", broker.url))?;
    Ok(format!(
        "{scheme}://{}:{}@{rest}",
        broker.username, broker.password
    ))
}

/// Strip URL-embedded credentials before logging.
fn redact_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        if let Some((_creds, host)) = rest.rsplit_once('@') {
            return format!("{scheme}://…@{host}");
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker(url: &str) -> BrokerProfile {
        BrokerProfile {
            url: url.to_string(),
            username: "station-17".to_string(),
            password: "s3cret".to_string(),
        }
    }

    #[test]
    fn profile_parses_the_043_sample() {
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
        assert_eq!(profile.app.name, "graz-balcony");
        assert_eq!(profile.app.lat, Some(47.07));
        assert_eq!(profile.app.lon, Some(15.44));
    }

    /// A hand-written profile may leave the location to the environment.
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

    /// 043 §5: services may add fields without a version bump.
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

    #[test]
    fn broker_url_splits_with_scheme_defaults() {
        assert_eq!(
            split_broker_url("mqtts://broker.example.com:8883").unwrap(),
            (true, "broker.example.com".to_string(), 8883)
        );
        assert_eq!(
            split_broker_url("mqtts://broker.example.com").unwrap(),
            (true, "broker.example.com".to_string(), 8883)
        );
        assert_eq!(
            split_broker_url("mqtt://localhost").unwrap(),
            (false, "localhost".to_string(), 1883)
        );
        assert!(split_broker_url("http://x").is_err());
        assert!(split_broker_url("no-scheme").is_err());
    }

    #[test]
    fn connector_url_carries_credentials() {
        assert_eq!(
            url_with_credentials(&broker("mqtts://broker.example.com:8883")).unwrap(),
            "mqtts://station-17:s3cret@broker.example.com:8883"
        );
    }

    #[test]
    fn redaction_hides_credentials() {
        assert_eq!(
            redact_url("mqtts://station-17:s3cret@broker.example.com:8883"),
            "mqtts://…@broker.example.com:8883"
        );
        assert_eq!(redact_url("mqtt://localhost"), "mqtt://localhost");
    }
}
