# weather-station-py

The pyo3 door onto `StationHandle`, built as a spike.

Design 008 §5.3 argues that the Python wheel should bind an API that already
exists rather than invent one, and §6 defers the wheel to a later tag. This
crate is neither the wheel nor a proposal for it: it is the experiment that
answers whether the blocking door survives contact with a foreign caller
**before** `aimdb-weather-station` and `aimdb-sync` reach a registry, where
changing either costs a version in two repositories.

It has no maturin metadata, no build matrix and no distribution name. Those are
release engineering, and they wait for the tag that ships the wheels.

## Running it

```
make spike
```

Needs `mosquitto` on the path — the script starts a broker of its own on a free
port, so it touches no deployment. It builds the module, loads it the way an
installed wheel would be imported, and exercises the door against the real
handshake.

## What it found

**`close()` is not a flush, and the loss is silent.** A reading published
immediately before `close()` reaches the broker one to four times in eight,
varying run to run. Both calls
return `Ok`. `publish_*` hands the value to the slot's buffer and returns; the
outbound link serializes and writes it on the runtime thread, and
`AimDbHandle::detach` signals shutdown and joins that thread without draining
what is in flight. Measured against a loopback broker with no TLS, the window
closes by **20 ms** — 8/8 arrive with that much grace, and a station that never
closes at all loses nothing. A real deployment's window is larger.

This is the symmetric twin of the startup race §5.3 already discharges. The gate
there proves the outbound links are subscribed before `open` returns; nothing
makes the matching promise at the other end. It matters more for FFI than for
Rust because publish-and-exit is the shape a Python station takes — a cron job
reading a sensor, a notebook cell — while a Rust station runs forever and never
notices.

Worth settling before the crates publish, since every candidate fix changes a
public signature: a drain inside `close()`, a `detach_timeout`-style grace
argument, or an explicit `flush()` that the caller is told to use.

**`StationHandle::close(self)` cannot be bound as it stands.** A `#[pymethods]`
method never receives `self` by value, so the pyclass holds
`Option<StationHandle>` and `close()` takes the handle out of it. That is a
small tax, but it is not free: idempotency, use-after-close and `__exit__` all
become the binding's problem rather than the crate's, and every FFI layer — the
C ABI included — will solve them again. See `PyStation` in `src/lib.rs`.

**`StationError` is `#[non_exhaustive]`, so a foreign mapping cannot be
exhaustive.** `to_py_err` classifies the variants into profile / broker /
everything-else, and needs a wildcard arm to compile. A variant added later
lands in the fallback silently, at which point a Python caller catching
`BrokerError` around a join stops catching a broker failure. The crate could
own the classification — a `kind()` accessor, or the mapping itself behind a
feature — which is again a published-API decision.

**The rest holds.** In particular:

| Claim | Result |
|---|---|
| `StationHandle` is `Send + Sync`, which `#[pyclass]` requires | holds — pinned by a compile-time assertion in `src/lib.rs` |
| Blocking calls can release the GIL | holds — a Python thread ticked 454 times while a join blocked 5 s on an unresponsive broker |
| The startup gate holds for a foreign caller | holds — 8/8 first readings arrive when close is not racing them |
| The error enum reaches Python as something actionable | holds, with the `#[non_exhaustive]` caveat above |
| One interpreter can hold several stations | holds — slots 17 and 18 publish independently |
| A worker thread can publish through a handle another thread opened | holds |
| The payload on the wire is the versioned contract shape | holds — `{"schema_version":2,"celsius":…}` on `station/17/temperature` |

Two smaller notes for whoever writes the wheel. The GIL-release call is
`Python::detach`, not the `Python::allow_threads` §5.3 names — pyo3 renamed it.
And `#[pymodule] fn weather_station` generates a module that shadows the
`weather_station` *crate* inside `src/lib.rs`, so the dependency needs `::`
prefixes; the wheel inherits this, since the module has to keep that name.
