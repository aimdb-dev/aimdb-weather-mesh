# weather-station-py

The pyo3 door onto `StationHandle`, and a station template over it. Not yet
shipped: no maturin metadata, no build matrix, no distribution name.

## Run the station

```
make station-py CONFIG=station.local.toml
```

`python/station.py` owns its loop and calls `publish_*` — what the blocking
door is for, where the Rust template hands aimdb two async sources instead.
Swap `fetch()` for a sensor read and the rest stands. `OPEN_METEO_URL` points
at a self-hosted Open-Meteo or a fake. Coordinates come from the profile's
`[app]`, then `WEATHER_LAT`/`WEATHER_LON`, then Vienna; half a pair is an
error. SIGINT and SIGTERM set an event the loop waits on and nothing else —
`close` being safe from another thread is not the same as safe from a handler.

Needs 3.11 for `tomllib`, or `pip install tomli` on 3.9/3.10, and the module on
`PYTHONPATH` until there is a wheel.

## Using it

```python
import logging, weather_station

logging.basicConfig(level=logging.INFO)
weather_station.init_logging()
logging.getLogger("aimdb_core").setLevel(logging.WARNING)  # station INFO stays

with weather_station.Station.open_profile("station.toml") as station:
    station.publish_temperature(21.5)
```

`open_profile` takes `str`, `pathlib.Path`, or any `os.PathLike`.

Errors classify by what you can do: `ProfileError` — fix the file.
`BrokerError` — fix the deployment. `StationError` — the base, and what to
catch when you cannot distinguish.

`init_logging()` installs a `log::Log`, not a subscriber: process-wide logging
is your application's decision, so nothing global is installed on your behalf
beyond the one logger `log` exists to hold. It returns `True` if this call
installed the bridge, `False` if one was already there. Events arrive under
Python levels and Rust module paths with `::` translated to `.`, so
`getLogger("aimdb_core")` is a parent of what aimdb emits. Its optional
`filter` is only the floor that keeps unwanted events from acquiring the GIL —
`level` and `target=level`, longest prefix wins, either spelling — and Python's
own levels do the fine-grained work above it.

## Rules

- **One station, many threads.** `Station` is `#[pyclass(frozen)]`: several
  threads may publish at once, and `close()` is safe to call while they do. It
  takes `&self`, is idempotent, and releases the GIL for the join. A `SIGINT`
  handler, or a `with` block ending mid-publish, needs all three.
- **Never hold the GIL while acquiring anything the runtime thread can block
  on.** After `init_logging`, that thread calls into Python to log, so the GIL
  must be outermost. `close()` wraps the join in `Python::detach`, and `closed`
  reads an atomic rather than the mutex `shutdown` holds — a getter waiting on
  that mutex under the GIL would deadlock against a shutdown waiting for the
  GIL. No signature can carry this.
- **A forked child is refused.** It inherits a station nobody pumps, so
  `closed` reports `True` and a publish raises rather than returning into a
  buffer nobody drains.

## Limits

- **`close()` is not a flush.** A reading published immediately before a close
  may not reach the broker. Stations publish on a cadence, so what is lost is a
  reading nobody would have read — but a station that publishes once and exits
  wants a delivery signal, and there is none yet.
- **Publish-only.** The consumer path will be a factory: `consumer(key)`
  returns a fresh object per call, each with its own cursor, kept on one
  thread. `SyncConsumer` reads take `&mut self`, so a consumer pyclass cannot
  be `frozen` the way `Station` is, and a shared one fails with "Already
  borrowed". It stays pull-based, so Rust never calls into Python for data and
  the GIL rule above stays trivially satisfied. Splitting a stream is the
  application's own business — every consumer sees every value, so one thread
  pulls into a `queue.Queue`. A caller needing more drops to `aimdb-core`
  directly, which is open to a **Rust** consumer but not to someone installing
  this wheel.

## Before shipping

- **`extension-module` is unconditional** in `Cargo.toml`, fine only because
  this crate has no Rust tests: `cargo test` builds a binary that must resolve
  CPython's symbols, and that feature keeps them out. The wheel wants it behind
  an optional feature.
- **The dependency graph is already wheel-shaped.** `weather-station` with
  `default-features = false` and without `init-tracing`, so neither `tracing`
  nor `tracing-subscriber` appears in `cargo tree`. The TLS backend is named
  here and is `rustls`: a wheel must not hand its host interpreter a second
  OpenSSL.
- **`#[pymodule] fn weather_station` shadows the `weather_station` crate**
  inside `src/lib.rs`, hence the `::` prefixes. The wheel inherits that.
