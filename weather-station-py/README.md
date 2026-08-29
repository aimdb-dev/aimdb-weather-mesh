# weather-station-py

The pyo3 door onto `StationHandle`, and a station template over it. Design 008
§5.3 argues the Python wheel should bind an API that already exists rather than
invent one; this is that API, bound.

No maturin metadata, no build matrix, no distribution name — those wait for the
tag that ships the wheels.

## The station

`python/station.py` is the station itself — the pendant of
`weather-station-openmeteo`, fed by the same API, needing no hardware:

```
make station-py CONFIG=station.local.toml
```

It owns its loop and calls `publish_*` when it has a reading, which is what the
blocking door is for; the Rust template hands aimdb two async sources instead
and lets the record graph drive them. Everything the mesh defines stays below
the boundary, so what a station of your own replaces is the fetch — swap
`fetch()` for a sensor read and the rest stands.

`OPEN_METEO_URL` points it at a self-hosted Open-Meteo, or at a fake for
testing. Coordinates come from the profile's `[app]`, then
`WEATHER_LAT`/`WEATHER_LON`, then Vienna; half a pair is an error rather than a
silent mix. SIGINT and SIGTERM set an event the loop waits on — the handler
does nothing else, since `close` being safe from another thread is not the same
as safe from a signal handler.

Needs 3.11 for `tomllib`, or `pip install tomli` on 3.9/3.10, and the module on
`PYTHONPATH` — which is the wheel's job, once there is a wheel.

## What a caller has to know

**One station, many threads.** `Station` is `#[pyclass(frozen)]`, so several
threads may publish through one station at once, and `close()` is safe to call
while they do — it takes `&self`, is idempotent, and releases the GIL for the
join. A `SIGINT` handler or a `with` block ending mid-publish is the shape that
needs all three.

**Never hold the GIL while acquiring anything the runtime thread can block on.**
Once `init_logging` is installed, aimdb's runtime thread calls into Python to
log, which creates a lock ordering where none existed: the GIL must be
outermost. So `close()` wraps the join in `Python::detach`, and `closed` reads
an atomic rather than the mutex `shutdown` holds — a getter that waited on that
mutex under the GIL would deadlock against a shutdown waiting for the GIL. Rust
has no GIL, so no signature carries this; it is written down in both crates.

**`init_logging()` installs a `log::Log`, not a subscriber.** An extension
module is a library inside somebody else's application, and process-wide logging
is the application's decision, so nothing global is installed on your behalf
beyond the one logger `log` exists to hold. It returns `True` if this call
installed the bridge and `False` if one was already there — `log::set_boxed_logger`
makes that decision once, in Rust, for this door and the C one alike. Events
arrive under Python levels and Rust module paths, with `::` translated to `.` so
`getLogger("aimdb_core")` is a parent of what aimdb emits:

```python
import logging, weather_station
logging.basicConfig(level=logging.INFO)
weather_station.init_logging()
logging.getLogger("aimdb_core").setLevel(logging.WARNING)  # station INFO stays
```

The optional `filter` is only the floor that keeps unwanted events from
acquiring the GIL: `level` and `target=level`, comma-separated, longest matching
prefix first, in either spelling — `info,aimdb_core.builder=debug` and
`aimdb_core::builder=debug` both work. The fine-grained work is Python's
`logging` levels above it.

**Errors are classified by what you can do about them.** `ProfileError` — fix
the file. `BrokerError` — fix the deployment. `StationError` — the base, and
what to catch when you cannot distinguish. The match that produces the
classification is exhaustive inside the crate that owns the error enum, so a
variant added later is a compile error there rather than a silent
reclassification at the boundary.

**`open_profile` takes anything path-like.** `str`, `pathlib.Path`, any
`os.PathLike`.

**`close()` is not a flush.** `publish_*` hands the value to the slot's buffer
and returns; the outbound link writes it on the runtime thread. A reading
published immediately before a close may not reach the broker. Stations publish
on a cadence, so what is lost is a reading nobody would have read; a station
that publishes once and exits wants a delivery signal, which does not exist yet.

**A `fork()`ed child is refused, not silently accepted.** The child inherits a
station whose graph nobody pumps, so `aimdb-sync` stamps a fork generation:
`closed` reports `True` and a publish raises rather than returning into a buffer
nobody drains.

## Notes for whoever writes the wheel

The GIL-release call is `Python::detach`, not the `Python::allow_threads` §5.3
names — pyo3 renamed it. `#[pymodule] fn weather_station` generates a module
that shadows the `weather_station` *crate* inside `src/lib.rs`, so the
dependency needs `::` prefixes; the wheel inherits that, since the module keeps
the name.

`extension-module` is unconditional in `Cargo.toml`, which is fine only because
this crate has no Rust tests: `cargo test` builds a binary that has to resolve
CPython's symbols, and that feature is what keeps them out. The first `#[test]`
that touches an interpreter fails at run time. pyo3 suggests an optional
feature for exactly this, and the wheel will want one.

`log` is named in this crate's manifest for the logging bridge, with its `std`
feature for `set_boxed_logger`. `tracing` and `tracing-subscriber` are no longer
named, and no longer reachable: this crate takes `weather-station` with
`default-features = false` and without `init-tracing`, so neither appears in
`cargo tree -p weather-station-py` at all. That is the shape a wheel wants — an
extension module has no business carrying a subscriber it must not install.

Turning the defaults off also means the TLS backend has to be named here. It is
`rustls`: a wheel must not hand its host interpreter a second OpenSSL. The C
door made the same call. Built with `native-tls` instead, it would link the
interpreter's system OpenSSL.

**The consumer path, when it lands: a factory.** `consumer(key)` returns a
fresh object per call, each with its own cursor, kept on one thread. Not a
style preference: `SyncConsumer`'s reads take `&mut self`, so a consumer
pyclass cannot be `frozen` the way `Station` is, and a blocking read parked in
`Python::detach` while another thread touches the same object fails with
"Already borrowed".

It stays pull-based (`get`, `get_with_timeout`, `try_get`, `get_latest`,
`get_latest_with_timeout`), so Rust never calls into Python for data and the
lock ordering above stays trivially satisfied. A Rust-side `on_reading(cb)`
would put a GIL acquisition on the one thread every shutdown waits for; that
ergonomic shape belongs in a Python layer over an iterator, in a mixed wheel.

Deliberately **not** carried: a shared consumer, even though `aimdb-sync`
offers `Arc<Mutex<SyncConsumer<T>>>` for splitting a stream. It costs little,
because every consumer sees every value — an application wanting each value at
exactly one worker pulls the stream on one thread into a `queue.Queue`.

A caller needing more drops to `aimdb-core` without `aimdb-sync` — available to
a **Rust** consumer, not to someone installing this wheel. For them this is the
ceiling, and that is the trade.
