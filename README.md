# aimdb-weather-mesh

Station templates and shared data contracts for the AimDB weather mesh — a
distributed weather network in which many small stations publish observations
over MQTT and one hub aggregates them into a queryable AimDB instance.

```
   ┌──────────────────────────┐   ┌──────────────────────────┐
   │ weather-station-openmeteo│   │ weather-station-… (yours)│
   │ slot 2                   │   │ slot 17                  │
   └────────────┬─────────────┘   └────────────┬─────────────┘
                │ publish station/<n>/…        │
                └──────────────┬───────────────┘
                               ▼
                      ┌────────────────┐
                      │  MQTT broker   │
                      └────────┬───────┘
                               │ subscribe station/+/…
                               ▼
                      ┌────────────────┐
                      │  weather-hub   │  derives dew point per slot
                      │  AimX :7433    │  serves record.list / record.get
                      └────────┬───────┘
                               │
                    aimdb CLI, dashboard, MCP
```

## Crates

| Crate | Role |
|---|---|
| [`weather-contracts`](weather-contracts) | The `Temperature`, `Humidity` and `DewPoint` schemas. Defines the wire format every station and the hub agree on. `no_std` compatible. |
| [`weather-station`](weather-station) | Mesh-join behaviour every station shares: the profile format, the `slot-<n>` identity, the broker handshake, and the slot's records with their outbound links. |
| [`weather-station-openmeteo`](weather-station-openmeteo) | Station template that needs no hardware: it fetches real observations from Open-Meteo for a location and publishes them into an assigned slot. |
| [`weather-station-knx`](weather-station-knx) | Station template fed by a real KNX installation: temperature and humidity read off the bus through a KNXnet/IP gateway, throttled, and published into an assigned slot. |
| [`weather-hub`](weather-hub) | Aggregating hub: a fixed pool of station slots, dew point derived per slot, exposed over AimX for the CLI and the dashboard. |
| [`weather-mesh-client`](weather-mesh-client) | Browser client: the wasm adapter fused with the mesh's contracts, plus the TypeScript facade published as `@aimdb/weather-mesh-client`. |

Copy a station out as the starting point for one of your own. What you copy is
the part that makes it your station — the poll loop, the bus decoding, the
publish cadence. What it joins the mesh with comes from `weather-station`, so
two stations cannot drift apart on the slot format, the profile version or the
revocation policy.

`weather-station` has three doors. `Station` is the default: join, supply one
async task per quantity, run — the shape `weather-station-openmeteo` uses.
`MeshSlot` hands the builder back unbuilt for stations whose readings arrive
*through* the record graph off a connector AimDB already speaks, which is what
`weather-station-knx` does with KNX. `StationHandle`, behind the `sync`
feature, is for a caller that owns its own loop and calls
`publish_temperature(21.5)` when it has a reading — a plain thread, or a
Python, C or C++ station reaching Rust through an FFI layer:

```bash
cargo test -p weather-station --features sync
# end-to-end against a broker:
mosquitto -p 1883 & cargo test -p weather-station --features sync -- --ignored
```

The crate is `no_std` with `alloc`, and everything above is behind the default
`tokio-runtime` feature. An MCU station turns it off and keeps what the mesh
actually defines — the profile tables, the `slot-<n>` identity, the record keys
and topics, and `configure_slot_records!` to put the records on its own builder
— bringing its own Embassy adapter and MQTT connector:

```bash
cargo check -p weather-station --no-default-features --target thumbv7em-none-eabihf
```

The pre-flight broker probe is its own `preflight` feature (on by default with
`tokio-runtime`). A host station wants it, so a revoked slot fails loudly at
startup; an MCU is better off in the connector's reconnect loop than carrying a
second MQTT client for one CONNECT.

## Data model

Each station owns a numbered *slot*. Slot number `<n>` fixes both the MQTT
topics the station publishes to and the record keys the hub registers:

| Record key | MQTT topic | Producer |
|---|---|---|
| `station.<n>.temperature` | `station/<n>/temperature` | station |
| `station.<n>.humidity` | `station/<n>/humidity` | station |
| `station.<n>.dew_point` | *(not published)* | hub, joined from the two above |

Payloads are JSON, encoded and decoded through the `Linkable` implementations in
`weather-contracts`. A payload that does not match the schema is rejected by the
hub's deserializer and logged against the record key it arrived on. See the
[contracts reference](weather-contracts/README.md) for the field-level format.

## Prerequisites

- Rust stable, edition 2021.
- A checkout of [aimdb](https://github.com/aimdb-dev/aimdb) as a **sibling
  directory** of this repository. All crates depend on the AimDB crates by
  relative path:

  ```
  <workspace>/
  ├── aimdb/                 # required
  └── aimdb-weather-mesh/    # this repository
  ```


- An MQTT broker for the hub and stations to meet on — a local `mosquitto` for
  development, or the mesh broker named in a station profile.

## Run a local mesh

Three terminals bring up a complete mesh without any cloud dependency:

```bash
# 1 — broker
mosquitto -p 1883

# 2 — hub with a small slot pool
MESH_SLOTS=8 MQTT_BROKER=localhost cargo run -p weather-hub

# 3 — station on slot 2 (see the station README for station.local.toml)
cargo run -p weather-station-openmeteo -- --config station.local.toml
```

For a KNX station without KNX hardware, [`tools/knx-sensor-sim.py`](tools/knx-sensor-sim.py)
stands in for the gateway and the sensors behind it:

```bash
python3 tools/knx-sensor-sim.py --ga 9/1/0=21.5 --ga 9/1/1=48 --interval 10
cargo run -p weather-station-knx -- --config station.local.toml
```

Inspect the result with the AimDB CLI, built with its `transport-tcp` feature:

```bash
aimdb --connect tcp://localhost:7433 record list
```

## Run against the public mesh

A station on the public mesh is configured entirely by a `station.toml` profile
that carries its slot, broker credential and location:

```bash
cargo run -p weather-station-openmeteo -- --config station.toml
```

The [station README](weather-station-openmeteo/README.md#station-profile)
documents every field of the profile.

Profiles hold a broker credential. `station*.toml` is git-ignored in this
repository; keep the file at mode `0600`.

## License

Apache-2.0. See [LICENSE](LICENSE).
