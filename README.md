# aimdb-weather-mesh
Runnable station templates and shared contracts for the AimDB weather mesh.

| Crate | What it is |
|---|---|
| [`weather-contracts`](weather-contracts) | The `Temperature`, `Humidity` and `DewPoint` schemas — the wire contract of the mesh, and the single source of truth for anyone (or any agent) reading its data. |
| [`weather-station-openmeteo`](weather-station-openmeteo) | Station template with no hardware: real Open-Meteo observations for your location, published into your assigned slot. |
| [`weather-hub`](weather-hub) | The aggregating hub — a pool of station slots, dew point derived per slot, served over AimX for `aimdb record list` and the dashboard. |

Join the mesh by getting a station profile and running a template against it:

```bash
aimdb join https://mesh.aimdb.dev                             # writes station.toml
cargo run -p weather-station-openmeteo -- --config station.toml
```

Each template's README covers its own prerequisites and what you own after
cloning.
