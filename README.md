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
| [`weather-station-openmeteo`](weather-station-openmeteo) | Station template that needs no hardware: it fetches real observations from Open-Meteo for a location and publishes them into an assigned slot. |
| [`weather-hub`](weather-hub) | Aggregating hub: a fixed pool of station slots, dew point derived per slot, exposed over AimX for the CLI and the dashboard. |

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
