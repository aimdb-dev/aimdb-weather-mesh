//! Open-Meteo as the station's data source.
//!
//! Free of AimDB and contract types: everything here is about fetching numbers
//! for a coordinate, so replacing this module with a sensor driver leaves the
//! record graph in `main.rs` untouched.

use aimdb_core::RuntimeContext;
use serde::Deserialize;

/// How long a fetched observation stays valid. Both sources poll on the same
/// cadence, so a reading fetched inside this window is reused by the second one.
const REFRESH_WINDOW_NANOS: u64 = 60 * 1_000_000_000;

/// One reading of the station's location. Temperature and humidity come from
/// the same observation and therefore share a timestamp, which the hub's
/// dew-point join over the two records relies on.
#[derive(Clone, Copy)]
pub struct Observation {
    /// Air temperature in degrees Celsius.
    pub temperature: f64,
    /// Relative humidity in percent.
    pub humidity: f64,
    /// Unix timestamp in milliseconds, taken when the reading was fetched.
    pub timestamp: u64,
}

pub struct OpenMeteoClient {
    http: reqwest::Client,
    lat: f64,
    lon: f64,
    /// Last observation with the monotonic reading it was fetched at.
    cached: tokio::sync::Mutex<Option<(u64, Observation)>>,
}

#[derive(Debug, Deserialize)]
struct OpenMeteoResponse {
    current: CurrentWeather,
}

#[derive(Debug, Deserialize)]
struct CurrentWeather {
    temperature_2m: f64,
    relative_humidity_2m: f64,
}

impl OpenMeteoClient {
    pub fn new(lat: f64, lon: f64) -> Self {
        Self {
            http: reqwest::Client::new(),
            lat,
            lon,
            cached: tokio::sync::Mutex::new(None),
        }
    }

    /// The current observation, fetched at most once per
    /// [`REFRESH_WINDOW_NANOS`].
    ///
    /// The lock is held across the HTTP call so the second source waits and
    /// then reads the observation the first one fetched, rather than issuing a
    /// duplicate request. A combined sensor such as a BME280 behaves the same
    /// way: temperature and humidity arrive in one transaction.
    pub async fn current(&self, ctx: &RuntimeContext) -> Result<Observation, reqwest::Error> {
        let mut cached = self.cached.lock().await;

        if let Some((fetched_at, obs)) = *cached {
            if ctx.time().now().saturating_sub(fetched_at) < REFRESH_WINDOW_NANOS {
                return Ok(obs);
            }
        }

        let obs = self.fetch(ctx).await?;
        *cached = Some((ctx.time().now(), obs));
        Ok(obs)
    }

    async fn fetch(&self, ctx: &RuntimeContext) -> Result<Observation, reqwest::Error> {
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,relative_humidity_2m",
            self.lat, self.lon
        );

        let resp: OpenMeteoResponse = self.http.get(&url).send().await?.json().await?;

        Ok(Observation {
            temperature: resp.current.temperature_2m,
            humidity: resp.current.relative_humidity_2m,
            timestamp: unix_millis(ctx),
        })
    }
}

/// Wall-clock milliseconds from the runtime, so the station keeps no clock of
/// its own (`unix_time` is `None` on a runtime without an RTC).
fn unix_millis(ctx: &RuntimeContext) -> u64 {
    ctx.time()
        .unix_time()
        .map(|(s, ns)| s.saturating_mul(1000) + (ns / 1_000_000) as u64)
        .unwrap_or(0)
}
