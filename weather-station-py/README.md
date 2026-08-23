# weather-station-py

The pyo3 door onto `StationHandle`, built as a spike.

Design 008 §5.3 argues the Python wheel should bind an API that already exists
rather than invent one, and §6 defers the wheel to a later tag. This crate is
the experiment that answers whether the blocking door survives contact with a
foreign caller **before** `aimdb-weather-station` and `aimdb-sync` reach a
registry, where changing either costs a version in two repositories.

No maturin metadata, no build matrix, no distribution name — those wait for the
tag that ships the wheels.

## Running it

```
make spike
```

Needs `mosquitto` and `mosquitto-clients` on the path. The script starts a
broker of its own on a free port, builds the module, loads it the way an
installed wheel would be imported, and exercises the door against the real
handshake.

## What it found

### Two things the crates had to change

**`StationHandle::close(self)` could not be bound, and the workaround was
broken.** A `#[pymethods]` method never receives `self` by value, so the
pyclass held `Option<StationHandle>` and `close()` took the handle out of it —
which made `close` a `&mut self` method. pyo3 tracks that with a runtime borrow
flag, and `Python::detach` releases the GIL but *not* the flag. So while any
thread sat in a blocking publish, `close()` could not get its exclusive borrow:

```
RuntimeError: Already borrowed
```

The flag is checked before the method body runs, so `inner.take()` never
happened either: the call failed *and* the station stayed open, still holding
its slot. Measured at 200/200 failed closes with one thread publishing in a
loop. Two threads publishing at once are both `&self` and share the borrow
fine — 200 publishes across 4 threads, all of them landed — so this was only
ever the shutdown path: a `SIGINT` handler, or a `with` block ending, while
sensor threads run.

Fixed in `weather-station`, not in the binding. `StationHandle` now has
`shutdown(&self)`, which is idempotent and safe to call during a publish, plus
`is_closed()`; `close(self)` delegates to it. The `db` field moved into a
`Mutex<Option<AimDbHandle>>`, and a publish never contends for that lock
because `SyncProducer` holds its own `Weak<AimDb>` and never goes through the
handle. The pyclass is `#[pyclass(frozen)]`, which drops the borrow flag
entirely, and the `Option`, the `handle()` helper and the idempotency bookkeeping
are all gone from the binding. **The C ABI layer inherits the fix** rather than
solving the same three problems again, which is what the first version of this
document predicted it would have to do.

**The module installed a process-wide logging subscriber.** `init_tracing`
called `tracing_subscriber::fmt().init()`, which writes to stderr behind
Python's back and *panics* if a subscriber already exists. That panic crosses
pyo3 as `pyo3_runtime.PanicException`, a `BaseException` — so `except Exception`
around logging setup does not catch it, and a Rust backtrace lands on stderr.
Python code configures logging twice all the time.

An extension module is a library inside somebody else's application, and
process-wide logging is the application's decision. The rest of the stack
already gets this right: `aimdb-core`'s `log_*` macros are a feature-gated
facade over `tracing`, and no aimdb library installs a subscriber. Only this
module broke the rule.

`init_tracing` is gone from the module, replaced by `init_logging()`: a
`tracing_subscriber::Layer` that forwards events into Python's `logging`. It
returns `True`/`False` rather than panicking. The Rust station binaries keep
`weather_station::init_tracing` — they *are* the application — and it now uses
`try_init()`, so a second call is a no-op rather than a panic across an FFI
boundary.

This gives aimdb's events **more** control than before, not less. The old
filter hardcoded `"…,aimdb_core=info,aimdb=info"` at build time; now each
subsystem is addressable at runtime from Python:

```python
import logging, weather_station
logging.basicConfig(level=logging.INFO)
weather_station.init_logging()
logging.getLogger("aimdb_core").setLevel(logging.WARNING)  # station INFO stays
```

One detail that is easy to get wrong: `tracing` targets are Rust module paths
(`aimdb_core::builder`), and `logging` splits its hierarchy on `.`. The layer
translates `::` to `.`, without which `getLogger("aimdb_core")` would not be a
parent of anything aimdb emits and setting a level on it would silently do
nothing. Events observed under `weather_station.broker`, `weather_station.slot`,
`aimdb_core.builder` and `aimdb_core.session.pump`.

### The rule the wheel has to carry

Once the bridge exists, aimdb's runtime thread calls into Python to log. That
creates a lock ordering where none existed, and it is stronger than "release
the GIL before joining a thread":

> **Never hold the GIL while acquiring anything the runtime thread can block
> on.** The GIL must be outermost.

So `close()` wraps the join in `Python::detach`, and `StationHandle::is_closed`
reads an `AtomicBool` rather than the mutex `shutdown` holds — a getter that
waited on that mutex under the GIL would deadlock against a shutdown waiting
for the GIL to be released. Inside `shutdown`, the guard is dropped with a
`let` before the join for the same reason: writing the same code as a `match`
on the `take()` would extend the guard to the end of the match, because Rust
extends scrutinee temporaries.

None of this is visible in `StationHandle`'s signature — Rust has no GIL, so no
type can carry the constraint. It is written down in both crates and exercised
by the spike's "lock ordering" round, which calls every entry point under a
watchdog while aimdb's runtime thread is logging.

### Accepted, not fixed

**`close()` is not a flush.** `publish_*` hands the value to the slot's buffer
and returns, while the outbound link writes it on the runtime thread, and
`AimDbHandle::detach` signals shutdown and joins without draining. How much
gets lost varies, which is the point: over eight rounds of eight
publish-then-close cycles against a loopback broker with no TLS, two to five of
the eight temperatures arrived and none to four of the humidities — the second
publish has less time and fares worse. A 20 ms grace closes the window in
practice; 8/8 arrive with one.

Stations are long-lived and publish on a cadence, so the reading lost to a
shutdown is one nobody would have read. A grace period inside `close()` was
implemented and then reverted: it would tax every close to improve the odds for
a shape the mesh does not have, and no wait makes delivery certain — nothing
between the buffer and the socket reports what was written. The connector
already publishes at QoS 1, so if publish-once-and-exit stations ever appear,
the answer is a delivery signal over a standard MQTT ACK topic, not a shutdown
that waits. `StationHandle::close` records this.

### Two things the binding got wrong on its own

**The base exception was really called `StationErrorPy`.** `create_exception!`
takes the Python class name from the Rust identifier, and that had to avoid
clashing with the imported `StationError`, so tracebacks named
`weather_station.StationErrorPy` — an attribute that is not on the module and
cannot be imported. Fixed by renaming the *Rust* import
(`StationError as CoreStationError`) and leaving the good name to the macro.
The spike now checks each exception's `__name__` against its module attribute.

**`open_profile` rejected a `pathlib.Path`.** The parameter was `&str`, so
`Path("station.toml")` raised `TypeError: 'PosixPath' object is not an instance
of 'str'` — and the spike worked around it without noticing, writing
`str(path)` at every call. It takes `std::path::PathBuf` now, which accepts
`str`, `Path` and anything `os.PathLike`. Nothing changed on the Rust side:
`StationHandle::open_profile` already took `impl AsRef<Path>`.

### `StationError` was `#[non_exhaustive]` with nothing to match on

A foreign mapping needed a wildcard arm, so a variant added later would land in
it silently and a caller catching `BrokerError` around a join would quietly stop
catching broker failures. Fixed in `weather-station`: `StationErrorKind`
classifies by what the caller can do — fix the file, fix the deployment, or
neither — and the match that produces it is exhaustive inside the crate that
owns the enum.

### The rest holds

| Claim | Result |
|---|---|
| `StationHandle` is `Send + Sync`, which `#[pyclass]` requires | holds — pinned by a compile-time assertion in `src/lib.rs` |
| Blocking calls can release the GIL | holds — a Python thread ticked 476 times while a join blocked 5 s on an unresponsive broker |
| The startup gate holds for a foreign caller | holds — 8/8 first readings arrive |
| The error enum reaches Python as something actionable | holds, via `kind()` |
| One interpreter can hold several stations | holds — slots 17 and 18 publish independently |
| A worker thread can publish through a handle another thread opened | holds |
| Several threads can publish through one handle *at once* | holds — 200 publishes on 4 threads, none refused |
| Shutting down while sensor threads publish | holds *after* the fix above — 11 ms, no reading refused for any reason but "closed" |
| Every entry point is deadlock-free while the runtime thread logs | holds — each returns in ≤ 11 ms under a watchdog |
| The payload on the wire is the versioned contract shape | holds — `{"schema_version":2,"celsius":…}` on `station/17/temperature` |

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

`tracing` and `tracing-subscriber` are named in this crate's manifest for the
logging layer. Neither is new to the build graph — `weather-station` already
pulls both — but a layer cannot be written against a dependency the manifest
does not declare.

Not yet answered, because the module is publish-only: the consumer path.
`SyncConsumer` is pull-based (`get`, `get_with_timeout`, `try_get`,
`get_latest`, `get_latest_with_timeout`) with no callbacks, so Python can pull
and Rust never has to call into Python for data — which keeps the lock ordering
above trivially satisfied. A Rust-side `on_reading(callback)` would not: it
puts a GIL acquisition on the one thread every shutdown path waits for. The
ergonomic shape belongs in a Python layer over an iterator, in a mixed wheel,
where the callback runs on an ordinary Python thread.
