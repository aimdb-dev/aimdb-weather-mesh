# weather-station-openmeteo

The no-hardware station template for the public weather mesh. It fetches real
observations from [Open-Meteo](https://open-meteo.com) for your station's
location and publishes them into the slot the mesh assigned you — nothing to
wire up, nothing to solder.

## What you own after cloning

Two small pieces, and they are the ones worth changing:

- **The data source** — [`src/open_meteo.rs`](src/open_meteo.rs), plus the
  `temperature_source` and `humidity_source` loops in
  [`src/main.rs`](src/main.rs) that `.source()` registers into the record
  graph. The module carries no AimDB or contract types, so replacing it with a
  sensor driver leaves the wiring below untouched; that is the point of the
  template.
- **The record wiring** — `Temperature` and `Humidity` are registered under
  `station.{slot}.…`, buffered in an `SpmcRing`, and linked out to
  `mqtt://station/{slot}/…` with the contract's own serializer. The Rust type
  *is* the wire contract, so a payload that does not match
  [`weather-contracts`](../weather-contracts) is rejected at the hub and told
  so, visibly.

Both sources share one `OpenMeteoClient`, so a poll cycle costs a single HTTP
request and both records carry the same timestamp — which is what the hub's
dew-point join over them assumes. A real sensor wants the same shape: a BME280
returns temperature and humidity in one transaction.

Timestamps come from `ctx.time()` rather than a clock of the station's own, so
the source functions port to a runtime without a system clock unchanged.

Everything else — profile parsing, the broker credential, the pre-flight
check — is mesh plumbing you should not have to think about.

**Dew point is not published here.** The hub derives `station.{slot}.dew_point`
per slot from the temperature and humidity you publish, so a station sending it
would only produce traffic nothing subscribes to.

## Prerequisites

- Rust stable (2021 edition).
- A station profile (`station.toml`) — see below.

## Run it

The target loop is three commands:

```bash
aimdb join https://mesh.aimdb.dev                             # writes station.toml
cargo run -p weather-station-openmeteo -- --config station.toml
```

`aimdb join` ships with the AimDB CLI; until it lands you can write the profile
by hand — the format is fixed by
[design 043 §4](https://github.com/aimdb-dev/aimdb/blob/main/docs/design/043-join-endpoint-v1.md):

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

`station_id` must be `slot-<n>`; the station derives its topics
(`station/<n>/…`) and record keys (`station.<n>.…`) from it. Unknown fields are
ignored, so the service can extend the profile without breaking you.

### Where the coordinates come from

`app.lat`/`app.lon` are the coordinates the mesh coarsened to 2 decimals
(~1 km) — precise location is never collected. They are optional, and the
station resolves its location in this order:

1. **`app.lat`/`app.lon` in the profile**, when present. A joined station
   reports from the location the mesh published for it, so an environment
   variable cannot move your dot on the map.
2. **`WEATHER_LAT`/`WEATHER_LON`**, for a hand-written profile that omits them:

   ```bash
   WEATHER_LAT=47.07 WEATHER_LON=15.44 \
     cargo run -p weather-station-openmeteo -- --config station.toml
   ```

3. **Vienna** (48.2082, 16.3738) when neither names a location.

Coordinates are taken as a pair from one source; setting only one of them is an
error rather than a silent mix. The startup log says which source won.

The file holds a broker credential: keep it out of version control and at mode
`0600` (`aimdb join` writes it that way).

## Verify locally, without the cloud

Against a local mosquitto and the [hub](../weather-hub) in mesh mode:

```bash
# terminal 1 — broker
mosquitto -p 1883

# terminal 2 — hub with a small slot pool
MESH_SLOTS=8 MQTT_BROKER=localhost cargo run -p weather-hub

# terminal 3 — this station, with a local profile
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
# no coordinates in the profile, so pick a city here (or omit for Vienna)
WEATHER_LAT=47.07 WEATHER_LON=15.44 \
  cargo run -p weather-station-openmeteo -- --config station.local.toml
```

Then watch the slot fill in through the hub's AimX endpoint (the CLI needs its
`transport-tcp` feature for a `tcp://` endpoint):

```bash
aimdb record list --connect tcp://localhost:7433
```

`station.2.temperature` and `station.2.humidity` carry what this station
published; `station.2.dew_point` is the hub's derivation of the two.

## When something goes wrong

The station probes the broker with a single CONNECT before it builds anything.
A credential the broker rejects stops the station immediately with the reason
instead of retrying forever in the background:

```
Error: the broker rejected this station's credential (NotAuthorized).
  The slot was likely revoked (silent for 30 days, or by the operator).
  Re-join the mesh for a fresh slot: aimdb join <provisioning-url>
```

Broker URLs are redacted before they reach the log, so a shared terminal or a
pasted issue never leaks the credential.
