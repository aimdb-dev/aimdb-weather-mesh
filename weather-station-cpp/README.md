# weather-station-cpp

The C ABI door onto `StationHandle`, built as a spike — and the C++ layer over
it, which is a header rather than a library for reasons this document is mostly
about.

The pendant of `weather-station-py`. That crate asked whether the blocking door
survives contact with a foreign caller before `aimdb-weather-station` and
`aimdb-sync` reach a registry; this one asks the same question for the language
where the boundary is a C ABI, no interpreter mediates, and the compiler stops
helping at the `extern "C"`.

No soname, no CMake package config, no generated header — those wait for the tag
that ships the library.

## Running it

```
make spike-cpp
```

Needs `mosquitto`, `mosquitto-clients` and a C++17 compiler. The spike starts a
broker of its own on a free port, builds the cdylib, links against it the way a
consuming build would, and exercises the door against the real handshake.

## The shape, and why it is this shape

Rust cannot export C++. No class, no `std::string`, no `std::function` survives
a library boundary, because none of them has an ABI that is stable even between
two builds of the same compiler. So there are two artifacts, not one:

- `include/weather_station.h` — the C ABI. Fourteen `ws_*` symbols, opaque
  pointer, status codes, no ownership that is not spelled out in prose.
- `include/weather_station.hpp` — RAII, a `std::filesystem::path` constructor,
  an exception hierarchy, a move-only `Station`. **Header-only**, compiled by
  the consumer's own toolchain, therefore always ABI-compatible with the
  consumer.

That is the same conclusion `weather-station-py` reached from the other end
("the ergonomic shape belongs in a Python layer over the module"), and it is
worth stating as a rule rather than a coincidence: *the FFI boundary carries the
mechanism; the language-shaped API is written in that language.*

The cdylib exports exactly the fourteen symbols and nothing else — Rust's
internals do not leak into the consumer's dynamic namespace. The `staticlib`
does: 47,878 defined text symbols, and a link line that fails without
`-lssl -lcrypto -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc`.

## What it found

### The Python door's fixes carried, and one of them is load-bearing here

`StationHandle::shutdown(&self)` — idempotent, safe during a publish — was fixed
in `weather-station` because a `#[pymethods]` method never receives `self` by
value. This layer inherits it, exactly as the Python README predicted the C ABI
layer would, and the inheritance shows in the signature: `ws_station_close`
takes `const ws_station*`, and the C++ `Station::close()` is a `const` method.
A `close` that needed exclusive access would have forced either a mutex in this
layer or a documented "stop your sensor threads first", and the shape that needs
it — a `SIGINT` handler closing a station whose sensor threads are running —
would have been the one it broke. Measured: `close()` returns in 10 ms with four
threads publishing, and no thread saw anything but `StationError`.

`is_closed()` reading an atomic rather than the mutex `shutdown` holds carried
too, and it matters here for a *different* reason than it did in Python. There
the hazard was the GIL: a getter that waited on that mutex under the GIL would
deadlock against a shutdown waiting for the GIL. Here there is no GIL, so the
same call is merely reentrant — and the spike makes it reentrant on purpose, by
installing a log sink that calls `ws_station_is_closed` and `ws_station_slot`
from inside the logging path, i.e. on aimdb's runtime thread, including while
that station's own shutdown is in flight. Both return; `close()` completes in
10 ms.

The classification `StationError::kind()` carried as well, and it is what keeps
`#[non_exhaustive]` from being a trap twice over: a C caller's `switch` has a
`default` arm too, so a variant added later would land in it just as silently as
a Rust wildcard would.

### What C adds that Python did not

**Nothing may unwind across the boundary.** pyo3 catches panics and raises
`PanicException`. There is no equivalent here — a Rust panic reaching a C++
frame is undefined behaviour — so every entry point wraps its body in
`catch_unwind` and reports `WS_ERR_PANIC` (measured, with a feature-gated
`ws_debug_panic`), and the C++ header's log trampoline is `noexcept` and catches
everything for the same reason in the other direction. A sink that throws on
every event is survivable; the spike runs one.

Two things that guard cannot do, and both are requirements on somebody else:

- It is compiled out by `panic = "abort"`. A consumer's profile silently turns
  every one of those catches into an abort of the whole C++ process.
- It does not stop the message. A panic writes its text and backtrace to
  **fd 2** — measured at 1,732 bytes — past the installed sink, because Rust's
  panic hook is process-global. Installing a hook here would be the same
  trespass `init_tracing` was in the Python module. So the only real fix is
  upstream: **the library must not panic.**

**Every argument is hostile.** No `Option`, no lifetime, no UTF-8 guarantee.
`NULL` arrives at every entry point, and a `const char*` that is not UTF-8
arrives at `ws_station_open_profile` — which cannot be represented as a Rust
`Path` at all, so it is refused rather than mangled. This is the pendant of the
Python door's `PathBuf` fix (`pathlib.Path` used to raise `TypeError`), and it
resolves in the opposite direction: Python's fix widened what the door accepts,
and C's answer has to narrow it, because there is nothing in a `const char*`
that says what encoding it is. On Windows, where the console hands out UTF-16, a
shipped library needs a `_w` entry point or a documented encoding rule.

**Ownership has to be prose.** `ws_station_free` is the one entry point that is
not thread-safe against the others, because it destroys what they share. In
Python the interpreter's reference count made this a non-question. The C++
header turns the prose back into a compile error — `Station` is move-only, so
two owners cannot exist — but the C ABI underneath cannot, and a caller that
uses it directly is on its own.

**A destructor is a shutdown.** `~Station` calls `ws_station_free`, which joins
aimdb's runtime thread. Two things had to hold and do: a station held in a
static and destroyed after `main` returns shuts down cleanly, and a destructor
never lets an exception out (it would `std::terminate` during unwinding).

### The one thing that was broken, and is now fixed

**After `fork()`, the child used to be told the station was open, its publish
returned success, and the reading was dropped.**

`fork` copies the address space but not the threads, so the child inherited a
`ws_station` whose graph nobody pumps. Measured against a live broker: the
parent's readings before and after the fork both arrived; the child's never did.
`is_closed()` reported open. `publish_temperature` returned `WS_OK`.

That was the failure the graph-start gate exists to prevent — `set()` returning
`Ok` into a buffer nobody reads — reappearing on the other side of a `fork`, and
it mattered more here than it would in Python: a daemon that double-forks, or a
supervisor that forks per job, is an ordinary C++ shape.

Running a *destructor* in the child found a second failure the first probe hid:
joining a `JoinHandle` for a thread that does not exist in this process panics
inside `std` with "threads should not terminate unexpectedly", putting a Rust
backtrace on stderr from inside `~Station`.

Both are fixed upstream, where they belonged. `aimdb-sync` now stamps a fork
generation — maintained by a `pthread_atfork` handler, so the check on the
publish path is a relaxed atomic load rather than a `getpid` that would cost
more than twice the publish it guards — and refuses a stale handle, producer or
consumer with `SyncError::ForkedChild`. `detach` and `Drop` release the join
handle instead of joining. `StationHandle::is_closed` reports closed in a child.
The round now reads:

```
  ok    the parent keeps publishing across the fork
  ok    a forked child is told the station is closed
  ok    a forked child's publish is refused, not silently dropped
  ok    no phantom reading reached the broker
```

Details, and why the fix could not live in this layer, in `REQUIREMENTS.md`
(CR-1) and `review.md` §6.

### Accepted, not fixed

**`close()` is not a flush**, exactly as in the Python door: `publish_*` hands
the value to the slot's buffer and returns, while the outbound link writes it on
the runtime thread. In this run, 5/8 temperatures and 2/8 humidities arrived
from eight publish-then-close cycles with no grace period; with a 20 ms grace,
8/8 of each. The numbers move between runs and machines — the same round in the
Python spike reported 8/8 with no grace on this host — which is the argument for
an ACK topic rather than a shutdown that waits, not against it.

**The log target reaches C unmodified.** The Python bridge rewrites `::` to `.`
because `logging` splits its hierarchy there and `getLogger("aimdb_core")` would
otherwise be a parent of nothing. C has no hierarchy, so a C caller gets
`aimdb_core::builder` and a `strncmp`. Nothing to fix; worth knowing that the
two doors deliberately differ here.

**The sink cannot be uninstalled.** `tracing`'s global subscriber is set for the
life of the process, so `callback` and `user_data` must outlive it — which in
practice means the library must not be `dlclose`d once `ws_init_logging` has run.
On glibc it currently cannot be: `dlclose` returns 0 and the library stays
mapped, because Rust's thread-locals give it TLS with destructors. That is a
platform accident this document is recording, not a property to rely on.

### The rest holds

| Claim | Result |
|---|---|
| `StationHandle` is `Send + Sync`, which a shared `const ws_station*` requires | holds — pinned by a compile-time assertion in `src/lib.rs` |
| A blocking call parks its own thread and no other | holds — a second thread ticked 492 times while an open blocked 5 s on an unresponsive broker |
| The startup gate holds for a foreign caller | holds — 8/8 first readings arrive |
| The error enum reaches C++ as something actionable | holds, via `kind()` → `ws_status` → the exception hierarchy |
| One process can hold several stations | holds — slots 17 and 18 publish independently |
| A worker thread can publish through a station another thread opened | holds |
| Several threads can publish through one station *at once* | holds — 200 publishes on 4 threads, none refused |
| Shutting down while sensor threads publish | holds — 10 ms, no reading refused for any reason but "closed" |
| Every entry point is deadlock-free while the runtime thread logs | holds — each returns in ≤ 12 ms under a watchdog |
| The sink is reentrant into the getters, from the runtime thread | holds — including during that station's own shutdown |
| A sink that throws does not unwind into Rust | holds — the trampoline catches |
| The payload on the wire is the versioned contract shape | holds — `{"schema_version":2,"celsius":…}` on `station/17/temperature` |
| The cdylib exports only the ABI | holds — 14 dynamic symbols, all `ws_*` |
| A pure C consumer can use the C header | holds |

## Notes for whoever ships the library

**Sizes.** Release cdylib 3.9 MB, 2.9 MB stripped. The `staticlib` is 167 MB as
an archive (debug), which is what a static consumer's link step chews through
rather than what it emits.

**OpenSSL is in the link line.** The cdylib needs `libssl.so.3` and
`libcrypto.so.3`, and a static consumer must pass `-lssl -lcrypto`. It arrives
from `rumqttc`'s `use-native-tls`, which the `preflight` feature pulls in for one
CONNECT. `weather-station`'s own docs already call the pre-flight "a second MQTT
client bought for a single round-trip" and gate it for MCUs; for a C++ consumer
it is also a system-OpenSSL ABI constraint on every build that links this
library, and a second OpenSSL in a process that already has one. Worth deciding
deliberately rather than inheriting.

**`-fno-exceptions` does not compile.** The C++ header throws, so a consumer
built without exceptions — not rare in embedded C++ shops — cannot use it. The C
header is fine for them. If that audience matters, the header needs a
status-returning variant alongside the throwing one.

**Generate the declarations, keep the contract.** `include/weather_station.h` is
hand-written here, and half of it is prose a generator cannot infer: which
functions are safe from which thread, how long a returned pointer lives, what
the callback must not do. A shipped library should generate the declarations
with cbindgen and keep a file like this one for the rest.

**`ws_abi_version` earns its keep.** A header and a library from different tags
must refuse each other at startup rather than disagree about a signature at run
time. It is the C pendant of the wheel's `abi3` decision: one artifact per
platform, and a way to tell whether it is the right one.

Not yet answered, because the door is publish-only: the consumer path.
`SyncConsumer`'s methods take `&mut self`, unlike `SyncProducer::set(&self)`, so
a `ws_consumer*` shared between two threads would be aliasing UB with nothing to
catch it — the exact asymmetry that made `close` a problem in Python, in a place
where C has no borrow flag to complain. See `REQUIREMENTS.md`.
