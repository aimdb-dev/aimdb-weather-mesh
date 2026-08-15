# weather-station-knx

A weather mesh station fed by a real KNX installation. Temperature and humidity
sensors on the KNX bus are read through a KNXnet/IP gateway and republished into
the slot the mesh assigned this station, where the
[hub](../weather-hub) derives dew point exactly as it does for any other
station.

From the hub's point of view this is an ordinary station publishing
`station/<n>/temperature` and `station/<n>/humidity` — nothing in
[`weather-contracts`](../weather-contracts) or the hub knows or cares that the
values came off a KNX bus.

```
  KNX sensors (TP)
        │
        ▼
  ┌───────────────┐   KNXnet/IP        ┌──────────────────────┐   MQTT
  │ KNX/IP gateway│ ──── UDP/3671 ───▶ │ weather-station-knx  │ ─────────▶ broker ──▶ weather-hub
  │ 192.168.1.4   │                    │  slot <n>            │  station/<n>/…
  └───────────────┘                    └──────────────────────┘
```

## How it works

The station is configured entirely by a profile file. At startup it:

1. Reads `station.toml`, checks `profile_version`, and derives its slot number
   `<n>` from `station_id = "slot-<n>"`.
2. Validates the `[knx]` table — both quantities present, group addresses
   parseable and distinct, datapoint types in the per-quantity allow-list.
3. Probes the broker with a single MQTT CONNECT to validate the credential.
4. Registers four records and runs them:

   | Record key | Written by | Buffer | Published to |
   |---|---|---|---|
   | `knx.temperature` | inbound link from `knx://<GA>` | `SingleLatest` | — |
   | `station.<n>.temperature` | throttling transform | `SpmcRing`, capacity 10 | `station/<n>/temperature` |
   | `knx.humidity` | inbound link from `knx://<GA>` | `SingleLatest` | — |
   | `station.<n>.humidity` | throttling transform | `SpmcRing`, capacity 10 | `station/<n>/humidity` |

The `knx.*` records are station-local and never leave the process. They carry
the bus reading verbatim, which is what makes the staging worth it: the throttle
between them and the mesh records decouples the KNX cadence from the mesh
cadence, so a chatty on-change sensor cannot flood the broker.

Timestamps come from `ctx.time()` rather than a clock of the station's own, and
the throttle is a pure function of them.

**Dew point is not published by the station.** The hub derives
`station.<n>.dew_point` per slot from the temperature and humidity it receives.

**The station never writes to the bus.** It builds no `GroupValueWrite` frames;
the KNX side is read-only.

## Prerequisites

- Rust stable, edition 2021.
- A sibling `aimdb` checkout — see the [repository README](../README.md#prerequisites) —
  with the `knx-pico` submodule initialised:

  ```bash
  git -C ../aimdb submodule update --init _external/knx-pico
  ```

  The workspace `[patch.crates-io]` points `knx-pico` at that submodule; the
  fork carries an `npdu_length = 1` panic fix, without which a malformed
  telegram from the bus aborts the station.
- A KNXnet/IP gateway reachable by **IP address** — hostnames are not resolved.
- A station profile (`station.toml`).

## Station profile

A superset of the [Open-Meteo station's](../weather-station-openmeteo/README.md#station-profile)
profile: a profile issued by the mesh provisioning service works unchanged once
a `[knx]` table is appended.

```toml
profile_version = 1
station_id = "slot-17"

[broker]
url = "mqtts://xxxx.eu-central-1.emqx.cloud:8883"
username = "station-17"
password = "…"

[app]
name = "graz-office"
lat = 47.07          # accepted and ignored
lon = 15.44

[knx]
gateway = "knx://192.168.1.4:3671"
min_publish_secs = 60          # optional, default 60

[knx.temperature]
group_address = "9/1/0"
dpt = "9.001"                  # optional, default "9.001"

[knx.humidity]
group_address = "9/1/1"
dpt = "9.007"                  # optional, default "9.007"
```

| Field | Required | Meaning |
|---|---|---|
| `profile_version` | yes | Must be `1`. |
| `station_id` | yes | `slot-<n>`, `<n>` a `u16`. Fixes the station's topics and record keys. |
| `broker.url` | yes | `mqtt://host[:port]` or `mqtts://host[:port]`. Port defaults to 1883 / 8883. |
| `broker.username` / `broker.password` | yes | Credential issued for the slot. |
| `app.name` | yes | Display name, used in logs. |
| `app.lat` / `app.lon` | no | **Parsed and ignored.** Kept so a mesh-issued profile deserializes unchanged; a KNX station has no location-dependent behaviour. |
| `knx.gateway` | yes | KNXnet/IP gateway URL, `knx://<ip>[:port]`. Port defaults to 3671. |
| `knx.min_publish_secs` | no | Throttle window, default `60`. `0` disables throttling. |
| `knx.temperature.group_address` | yes | 3-level group address, `main/middle/sub`. |
| `knx.temperature.dpt` | no | Default `"9.001"`. |
| `knx.humidity.group_address` | yes | 3-level group address. The table is **not** optional. |
| `knx.humidity.dpt` | no | Default `"9.007"`. |

Unknown fields are ignored, so the provisioning service can extend the profile
without breaking existing stations.

The file contains a broker credential: keep it out of version control
(`station*.toml` is git-ignored here) and at mode `0600`.

### Both quantities are required

A profile with only `[knx.temperature]` is rejected at startup. Every slot in
the mesh has to be able to produce dew point, and a station publishing only
temperature leaves the hub permanently unable to derive
`station.<n>.dew_point`. A slot that is silently two-thirds empty is harder to
diagnose than one that refused to start.

Temperature and humidity are also the only quantities this station handles. A
KNX weather station often carries wind speed (DPT 9.005), illuminance (9.004)
and a rain contact (DPT 1.x) as well — all decodable, but there is no mesh
contract to publish them into.

### Datapoint types

The allow-list is per quantity, so a swapped configuration fails at startup
rather than publishing nonsense into the mesh:

| Quantity | Accepted `dpt` | Yields |
|---|---|---|
| temperature | `"9.001"` (default) | °C, from a 2-byte KNX float |
| humidity | `"9.007"` (default) | %, from a 2-byte KNX float |
| humidity | `"5.001"` | %, from a 1-byte scaled percentage |

DPT 5.001 carries percent in a single octet scaled over 0–255, so it resolves to
about 0.4 % and the decoded value is truncated to a whole percent: a sensor
reporting 48 % arrives as 47 %. That is the datapoint type, not the station —
use `9.007` where the extra resolution matters.

`9.004` is a perfectly valid datapoint type — it is illuminance — so it is
rejected for both, with the accepted values named:

```
Error: [knx.humidity] dpt = "9.004" is not a humidity type — use "9.007" (2-byte float) or "5.001" (1-byte percent)
```

**A telegram that does not decode is dropped, never coerced.** It is logged at
warn against its group address and the value never enters the buffer. Coercing
a decode failure to `0.0` would publish 0 °C into the mesh and drag the hub's
derived dew point with it.

## The throttle

`min_publish_secs` bounds how often each mesh record publishes, independently of
how fast the bus sends. The first value always passes; after that, a value
timestamped less than the window after the last published one is dropped. `0`
publishes every telegram.

It is **decimation, not aggregation**: an admitted value is the latest reading,
not a mean over the window. Averaging is arguably better data, but it would
change what `station.<n>.temperature` means relative to every other station in
the mesh.

A timestamp that moves backwards — a clock step, an NTP correction — restarts
the window rather than muting the record until wall time catches up.

## Environment variables

| Variable | Default | Effect |
|---|---|---|
| `RUST_LOG` | `weather_station_knx=info,aimdb_core=info,aimdb=info` | Tracing filter. The `aimdb` target carries the per-telegram accept/reject reports; `weather_station_knx=debug` additionally logs each value the throttle suppresses. |

## Run it

Against the mesh:

```bash
cargo run -p weather-station-knx -- --config station.toml
```

### Locally, with no KNX hardware

[`tools/knx-sensor-sim.py`](../tools/knx-sensor-sim.py) is a KNXnet/IP gateway
with sensors behind it: it answers CONNECT / CONNECTIONSTATE and then originates
`TUNNELING_INDICATION` frames on a timer, the way a sensor configured for cyclic
sending does. (The simulator shipped with `knx-pico` is a loopback echo — it
only reflects group *writes*, so it can never drive a read-only station.)

Four terminals bring up the whole path:

```bash
# 1 — broker
mosquitto -p 1883

# 2 — hub with a small slot pool
MESH_SLOTS=8 MQTT_BROKER=localhost cargo run -p weather-hub

# 3 — the KNX side: 21.5 °C on 9/1/0, 48 % on 9/1/1, every 10 s
python3 tools/knx-sensor-sim.py --ga 9/1/0=21.5 --ga 9/1/1=48 --interval 10

# 4 — this station
cat > station.local.toml <<'TOML'
profile_version = 1
station_id = "slot-2"

[broker]
url = "mqtt://localhost:1883"
username = "station-2"
password = "local"

[app]
name = "local-test"

[knx]
gateway = "knx://127.0.0.1:3671"
min_publish_secs = 30

[knx.temperature]
group_address = "9/1/0"

[knx.humidity]
group_address = "9/1/1"
TOML

cargo run -p weather-station-knx -- --config station.local.toml
```

The station reports every telegram it accepts, and the throttle holds the
publish rate at 30 s despite the 10 s simulator interval:

```
📡 KNX gateway: knx://127.0.0.1:3671
   9/1/0 (DPT 9.001) → station.2.temperature → mqtt://station/2/temperature
   9/1/1 (DPT 9.007) → station.2.humidity    → mqtt://station/2/humidity
   throttle: at most one publish per 30s per record
…
INFO aimdb: 🌡️  KNX 9/1/0 → 21.5°C
INFO aimdb: 💧 KNX 9/1/1 → 48.0%
```

Watch the slot fill in through the hub's AimX endpoint. The AimDB CLI needs its
`transport-tcp` feature for a `tcp://` endpoint:

```bash
aimdb --connect tcp://localhost:7433 record get station.2.dew_point
```

The simulator can also exercise the rejection path — `--malformed-every 4`
truncates every fourth telegram, which the station must log and drop without
publishing:

```bash
python3 tools/knx-sensor-sim.py --ga 9/1/0=21.5 --ga 9/1/1=48 --malformed-every 4
```

`--ga 9/1/1=48:5.001` sends humidity as a 1-byte percentage instead, to check a
profile that sets `dpt = "5.001"`. Run `--help` for the rest.

## Adapting it

- [`src/knx.rs`](src/knx.rs) is the `[knx]` profile table and its validation.
  Adding a quantity starts here.
- [`src/dpt.rs`](src/dpt.rs) maps a profile string to a decoder. Both decoders
  yield `f32` in the unit the contract expects, so the record wiring does not
  care which was configured.
- [`src/main.rs`](src/main.rs) holds the record graph. Publishing a third
  quantity means a contract for it in `weather-contracts` and a record for it at
  the hub — see [Out of scope](#known-limitations).

Writing *back* to KNX — the hub's dew point to a display group address — would
be an inbound MQTT link plus an outbound KNX link with `Dpt9::Temperature`'s
encoder. Deliberately not done here: this station is read-only.

## Known limitations

| Limitation | Cause | What to do |
|---|---|---|
| **No value until the bus sends.** After startup the records are empty and the slot looks dead to the hub until the first telegram arrives. | The connector builds only `GroupValueWrite` frames — there is no `GroupValueRead` to poll current state on connect. | Configure the sensors for cyclic sending in ETS (typically every 5–15 min). |
| **No heartbeat republish.** If the bus goes silent the mesh record stops updating; the station does not re-emit the last known value. | The transform is purely reactive, and a `.source()` cannot read another record nor coexist with a transform on the same record. | As above — cyclic sending in ETS. |
| **Temperature and humidity timestamps are independent.** | They are separate KNX sensors on their own schedules. | Harmless: the hub's dew-point join keeps the last value of each input and takes `max(timestamps)`. |
| **No KNX Secure.** | Not supported by `aimdb-knx-connector` (plaintext KNXnet/IP only). | Keep the station and gateway on a trusted LAN segment. |
| **The gateway must be an IP address.** | The connector does not resolve hostnames. | Use the gateway's address in `knx.gateway`. |

There is deliberately **no gateway pre-flight** to match the broker one: the
connector's retry loop is the right behaviour for a LAN device that reboots, and
a station that refused to start because the gateway was briefly down would be
worse. The startup route table and the per-telegram logs are what tell you the
KNX side is healthy.

## Troubleshooting

The station prints errors as a single `Error: …` line and exits with status 1.

| Message | Cause | Fix |
|---|---|---|
| `[knx.humidity] is missing` | The profile names only one quantity. | Add the table; both are required. |
| `[knx.temperature] group_address: '32/0/0': main group 32 is out of range (0–31)` | A group address level outside what KNX encodes. | Correct the address; ranges are 0–31 / 0–7 / 0–255. |
| `[knx.humidity] dpt = "…" is not a humidity type` | A datapoint type belonging to another quantity. | Use one of the accepted values the message lists. |
| `[knx.temperature] and [knx.humidity] both read group address …` | One address configured for both quantities. | Give each quantity its own sensor's address. |
| `Invalid KNX gateway address …` | A hostname, or a malformed address, in `knx.gateway`. | Use the gateway's IP address. |
| `the broker rejected this station's credential (NotAuthorized)` | The slot was revoked — by the operator, or after 30 days of silence. | Re-join the mesh for a fresh slot. |
| `cannot reach the broker at …` / `timed out connecting to the broker at …` | Broker unreachable, wrong host or port, or TLS not available on the port. | Check `broker.url`; the pre-flight probe times out after 15 s. |
| `station_id '…' is not of the form slot-<n>` | The profile is not from a weather-mesh deployment. | Use a profile issued by the mesh, or fix `station_id`. |
| `unsupported profile_version N` | The profile is newer than this station. | Update the station, or re-issue the profile. |
| `knx 9/1/0: undecodable telegram …` (warning, station keeps running) | A telegram on that address does not carry a value of the configured DPT. | Check the `dpt` against what the sensor actually sends. |

**The station starts, connects to everything, and publishes nothing.** This is
almost always a group address that no sensor sends on. Compare the startup route
table against ETS; if no `🌡️`/`💧` line ever appears, no telegram is arriving.
`RUST_LOG=weather_station_knx=debug` additionally reports every value the
throttle suppresses, which separates "nothing arriving from KNX" from "arriving
but suppressed".

The broker credential is stripped from any URL before it reaches the log, so
startup output and pasted logs do not leak it.
