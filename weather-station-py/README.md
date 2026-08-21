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

Needs `mosquitto` on the path. The script starts a broker of its own on a free
port, builds the module, loads it the way an installed wheel would be imported,
and exercises the door against the real handshake.

## What it found

**`close()` is not a flush.** A reading published in the last milliseconds
before `close()` reaches the broker one to four times in eight; both calls
return `Ok`. `publish_*` hands the value to the slot's buffer and returns,
while the outbound link writes it on the runtime thread, and
`AimDbHandle::detach` signals shutdown and joins without draining. The window
closes by 20 ms against a loopback broker with no TLS.

**Accepted, not fixed.** Stations are long-lived and publish on a cadence, so
the reading lost to a shutdown is one nobody would have read. A grace period
inside `close()` was implemented and then reverted: it would tax every close to
improve the odds for a shape the mesh does not have, and no wait makes delivery
certain — nothing between the buffer and the socket reports what was written.
The connector already publishes at QoS 1, so if publish-once-and-exit stations
ever appear, the answer is a delivery signal over a standard MQTT ACK topic,
not a shutdown that waits. `StationHandle::close` records this.

**`StationHandle::close(self)` cannot be bound as it stands.** A `#[pymethods]`
method never receives `self` by value, so the pyclass holds
`Option<StationHandle>` and `close()` takes the handle out of it. Idempotency,
use-after-close and `__exit__` become the binding's problem rather than the
crate's, and every FFI layer — the C ABI included — will solve them again.

**`StationError` was `#[non_exhaustive]` with nothing to match on.** A foreign
mapping needed a wildcard arm, so a variant added later would land in it
silently and a caller catching `BrokerError` around a join would quietly stop
catching broker failures. Fixed in `weather-station`: `StationErrorKind`
classifies by what the caller can do — fix the file, fix the deployment, or
neither — and the match that produces it is exhaustive inside the crate that
owns the enum.

**The rest holds.**

| Claim | Result |
|---|---|
| `StationHandle` is `Send + Sync`, which `#[pyclass]` requires | holds — pinned by a compile-time assertion in `src/lib.rs` |
| Blocking calls can release the GIL | holds — a Python thread ticked 420 times while a join blocked 5 s on an unresponsive broker |
| The startup gate holds for a foreign caller | holds — 8/8 first readings arrive |
| The error enum reaches Python as something actionable | holds, via `kind()` |
| One interpreter can hold several stations | holds — slots 17 and 18 publish independently |
| A worker thread can publish through a handle another thread opened | holds |
| The payload on the wire is the versioned contract shape | holds — `{"schema_version":2,"celsius":…}` on `station/17/temperature` |

Two notes for whoever writes the wheel. The GIL-release call is
`Python::detach`, not the `Python::allow_threads` §5.3 names — pyo3 renamed it.
And `#[pymodule] fn weather_station` generates a module that shadows the
`weather_station` *crate* inside `src/lib.rs`, so the dependency needs `::`
prefixes; the wheel inherits that, since the module keeps the name.
