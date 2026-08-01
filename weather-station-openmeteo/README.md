# weather-station-openmeteo

A weather mesh station that needs no hardware. It fetches current observations
from [Open-Meteo](https://open-meteo.com) for a set of coordinates and publishes
them as `Temperature` and `Humidity` records into the slot the mesh assigned it.

Use it to join the mesh without a sensor, or as the starting point for a station
that reads real hardware — see [Replacing the data
source](#replacing-the-data-source).

## How it works

The station is configured entirely by a profile file. At startup it:

1. Reads `station.toml`, checks `profile_version`, and derives its slot number
   `<n>` from `station_id = "slot-<n>"`.
2. Resolves the coordinates to fetch weather for (see [Location](#location)).
3. Probes the broker with a single MQTT CONNECT to validate the credential
   before building anything.
4. Registers two records and runs them:

   | Record key | MQTT topic | Buffer |
   |---|---|---|
   | `station.<n>.temperature` | `station/<n>/temperature` | `SpmcRing`, capacity 10 |
   | `station.<n>.humidity` | `station/<n>/humidity` | `SpmcRing`, capacity 10 |

Each record is fed by a `.source()` poll loop and linked outbound to its topic
with the contract's own serializer, so the published payload is exactly what
[`weather-contracts`](../weather-contracts) defines and the hub accepts.

Both loops poll every **5 minutes** (`POLL_INTERVAL_SECS`) and share one
`OpenMeteoClient`. The client caches an observation for 60 seconds, so a poll
cycle costs a single HTTP request and both records carry the same timestamp —
the hub's dew-point join over the two records depends on that pairing.
Timestamps come from `ctx.time()` rather than a clock of the station's own, so
the source functions also work on a runtime without a system clock.

**Dew point is not published by the station.** The hub derives
`station.<n>.dew_point` per slot from the temperature and humidity it receives.

## Prerequisites

- Rust stable, edition 2021.
- A sibling `aimdb` checkout — see the [repository README](../README.md#prerequisites).
- A station profile (`station.toml`).

## Station profile

The profile is a TOML file:

```toml
profile_version = 1
station_id = "slot-17"

[broker]
url = "mqtts://xxxx.eu-central-1.emqx.cloud:8883"
username = "station-17"
password = "…"

[app]
name = "graz-balcony"
lat = 47.07
lon = 15.44
```

| Field | Required | Meaning |
|---|---|---|
| `profile_version` | yes | Must be `1`. Any other value is rejected at startup. |
| `station_id` | yes | Must be `slot-<n>`, `<n>` a `u16`. Fixes the station's topics and record keys. |
| `broker.url` | yes | `mqtt://host[:port]` or `mqtts://host[:port]`. Port defaults to 1883 / 8883. |
| `broker.username` | yes | MQTT username issued for the slot. |
| `broker.password` | yes | MQTT password issued for the slot. |
| `app.name` | yes | Display name of the station, used in logs. |
| `app.lat`, `app.lon` | no | Coordinates to fetch weather for. See [Location](#location). |

Unknown fields are ignored, so the provisioning service can extend the profile
without breaking existing stations.

The file contains a broker credential: keep it out of version control
(`station*.toml` is git-ignored here) and at mode `0600`.

## Location

Coordinates are resolved from the first source that supplies them, and the
startup log reports which one won:

| Priority | Source | Notes |
|---|---|---|
| 1 | `app.lat` / `app.lon` in the profile | The mesh publishes coordinates coarsened to 2 decimals (~1 km); precise location is never collected. A joined station always reports from the location the mesh published for it. |
| 2 | `WEATHER_LAT` / `WEATHER_LON` | For a hand-written profile that omits coordinates. |
| 3 | Vienna, 48.2082°N 16.3738°E | Fallback when neither source names a location. |

Coordinates are taken as a pair from a single source. Setting only one of
`app.lat` / `app.lon`, or only one of `WEATHER_LAT` / `WEATHER_LON`, is an error
rather than a silent mix, as is a non-numeric value in either variable.

## Environment variables

| Variable | Default | Effect |
|---|---|---|
| `WEATHER_LAT`, `WEATHER_LON` | unset | Coordinates used when the profile carries none. Set both or neither. |
| `RUST_LOG` | `weather_station_openmeteo=info,aimdb_core=info,aimdb=info` | Tracing filter. The `aimdb` target carries what the source loops report through `ctx.log()`. |

## Run it

Against the mesh:

```bash
cargo run -p weather-station-openmeteo -- --config station.toml
```

Against a local broker and [hub](../weather-hub), with no cloud involved:

```bash
# terminal 1 — broker
mosquitto -p 1883

# terminal 2 — hub with a small slot pool
MESH_SLOTS=8 MQTT_BROKER=localhost cargo run -p weather-hub

# terminal 3 — this station
cat > station.local.toml <<'TOML'
profile_version = 1
station_id = "slot-2"

[broker]
url = "mqtt://localhost:1883"
username = "station-2"
password = "local"

[app]
name = "local-test"
TOML

# no coordinates in the profile, so name a location here (or omit for Vienna)
WEATHER_LAT=47.07 WEATHER_LON=15.44 \
  cargo run -p weather-station-openmeteo -- --config station.local.toml
```

Watch the slot fill in through the hub's AimX endpoint. The AimDB CLI needs its
`transport-tcp` feature for a `tcp://` endpoint:

```bash
aimdb --connect tcp://localhost:7433 record list
```

`station.2.temperature` and `station.2.humidity` carry what this station
published; `station.2.dew_point` is the hub's derivation from the two.

## Replacing the data source

The template is arranged so that swapping Open-Meteo for a real sensor touches
two files and leaves the record wiring alone:

- [`src/open_meteo.rs`](src/open_meteo.rs) contains the data source and uses no
  AimDB or contract types. Replacing it with a sensor driver leaves everything
  below it unchanged.
- The `temperature_source` and `humidity_source` loops in
  [`src/main.rs`](src/main.rs) convert an observation into contract types and
  call `producer.produce(…)`. Replace the `client.current(&ctx)` call with a
  sensor read; the surrounding `.source()` registration, buffering and outbound
  link stay as they are.

A driver that returns temperature and humidity from one transaction — a BME280,
for instance — fits the existing shape directly: both sources read from the same
shared client and therefore emit matching timestamps.

## Troubleshooting

The station prints errors as a single `Error: …` line and exits with status 1.

| Message | Cause | Fix |
|---|---|---|
| `the broker rejected this station's credential (NotAuthorized)` | The slot was revoked — either by the operator, or automatically after 30 days of silence. | Re-join the mesh for a fresh slot. |
| `cannot reach the broker at …` / `timed out connecting to the broker at …` | Broker unreachable, wrong host or port, or TLS not available on the port. | Check `broker.url`; the pre-flight probe times out after 15 s. |
| `station_id '…' is not of the form slot-<n>` | The profile is not from a weather-mesh deployment. | Use a profile issued by the mesh, or fix `station_id`. |
| `unsupported profile_version N` | The profile is newer than this station. | Update the station, or re-issue the profile. |
| `station.toml sets only one of app.lat / app.lon` | Half a coordinate pair. | Give both values or neither. |
| `WEATHER_LAT='…' is not a number` | Unparseable coordinate in the environment. | Correct or unset the variable. |
| `Open-Meteo fetch failed: …` (warning, station keeps running) | Transient HTTP or API failure. | None — the next poll retries in 5 minutes. |

The broker credential is stripped from any URL before it reaches the log, so
startup output and pasted logs do not leak it.
