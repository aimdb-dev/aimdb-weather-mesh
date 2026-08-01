# weather-hub

The aggregating node of the weather mesh. It subscribes to every station slot on
the MQTT broker, derives dew point per slot, and serves the resulting records
over AimX for the AimDB CLI, the dashboard and MCP clients.

## How it works

The hub registers a **fixed pool of slots** at startup — `MESH_SLOTS` slots, each
with three records:

| Record key | Source | Buffer |
|---|---|---|
| `station.<n>.temperature` | inbound link from `station/<n>/temperature` | `SpmcRing`, capacity 100 |
| `station.<n>.humidity` | inbound link from `station/<n>/humidity` | `SpmcRing`, capacity 100 |
| `station.<n>.dew_point` | `transform_join` over the two records above | `SpmcRing`, capacity 100 |

Because every slot's records exist from startup, admitting a station is only a
matter of handing out an unused slot number — no hub restart or recompile is
involved. The hub registers `3 × MESH_SLOTS` records (192 at the default of 64).

All records call `.observe()`, which folds each value into the record's signal
gauge (last / min / max / mean) behind `record list` and `record get`, and
`.with_remote_access()`, which installs the JSON codec that AimX
`record.get` / `record.subscribe` read through. Individual values are not
logged: across dozens of slots, per-message console lines drown out everything
else.

### Schema enforcement

Inbound payloads are decoded by the contracts' `Linkable::from_bytes`. A payload
that does not match the schema is rejected and logged against the key it arrived
on:

```
station.2.temperature: rejected payload: Migration error: …
```

The record keeps its previous value; a malformed station cannot corrupt the slot.

### Dew point

`station.<n>.dew_point` is derived at the hub rather than sent by the station, so
the dashboard stays supplied by stations that publish only temperature and
humidity. The join holds the latest reading of each input and produces a new dew
point whenever either changes, once both are present. It uses the Magnus
approximation, `T_dp ≈ T − (100 − RH) / 5`, and stamps the result with the newer
of the two contributing timestamps.

### AimX endpoint

The hub exposes a TCP AimX server (`aimdb-tcp-connector`). It is read-only under
the default policy and binds to loopback unless `AIMX_BIND` says otherwise; the
hosted deployment fronts it as `aimdb.dev:7433`.

## Configuration

The hub is configured entirely through the environment.

| Variable | Default | Effect |
|---|---|---|
| `MQTT_URL` | — | Full connector URL, including credentials: `mqtts://user:pass@host:8883`. Takes precedence over `MQTT_BROKER`. |
| `MQTT_BROKER` | `localhost` | Host name for a credential-free local broker; expands to `mqtt://<host>`. Ignored when `MQTT_URL` is set. |
| `MESH_SLOTS` | `64` | Number of station slots to register. |
| `AIMX_BIND` | `127.0.0.1:7433` | Bind address of the AimX TCP endpoint. Use `0.0.0.0:7433` to accept remote clients. |
| `RUST_LOG` | `weather_hub=info,aimdb_core=info,aimdb=info` | Tracing filter. The `aimdb` target carries the contract-violation reports from the inbound deserializers. |

The hub connects with MQTT client id `weather-hub`. Only the host part of the
broker URL is logged, so a credential in `MQTT_URL` does not reach the log.

## Prerequisites

- Rust stable, edition 2021.
- A sibling `aimdb` checkout — see the [repository README](../README.md#prerequisites).
- An MQTT broker.

## Run it

```bash
# local broker, small pool
mosquitto -p 1883 &
MESH_SLOTS=8 MQTT_BROKER=localhost cargo run -p weather-hub
```

Against the mesh broker:

```bash
MQTT_URL='mqtts://hub-sub:…@xxxx.eu-central-1.emqx.cloud:8883' \
AIMX_BIND=0.0.0.0:7433 \
  cargo run -p weather-hub
```

Then point the AimDB CLI at the endpoint. The CLI needs its `transport-tcp`
feature for a `tcp://` endpoint:

```bash
aimdb --connect tcp://localhost:7433 record list
aimdb --connect tcp://localhost:7433 record get station.2.dew_point
aimdb --connect tcp://localhost:7433 watch station.2.temperature
```

Every configured slot is listed by `record list` from startup, whether or not a
station is publishing into it; unclaimed slots simply carry no value yet. To feed
one, run a [station](../weather-station-openmeteo) against the same broker.
