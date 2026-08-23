# What the C++ door needs from aimdb core

Derived the same way `weather-station-py` derived its list: by building the
door, running it against a real broker, and writing down what the crates
underneath had to be for it to work — except where they were not, in which case
it is written down here.

Each item says what it needs, what showed the need, and where it lands.
"Evidence" is either a spike round (`make spike-cpp`) or a file and line in the
sibling aimdb checkout, and the two are labelled differently on purpose:
**measured** means the spike reproduces it, **read** means it was found in the
source and has not been triggered on demand.

Numbering is `CR-n` (core requirement). Nothing here is a request to change
`weather-station` — the fixes the Python door needed there all carried, and the
prediction that the C ABI layer would inherit them rather than re-solve them
held.

---

## A. Release-gating

These four change what a correct FFI layer can promise, so they belong in the
release rather than after it.

### CR-1 — A `fork()`ed child must not be told its station is open

**Measured.** Round *"after fork(), the child has no runtime thread"*. The child
of a `fork` inherits the handle but not the runtime thread. It is told
`is_closed() == false`, its publish returns `Ok`, and the reading is discarded:
the parent's readings before and after the fork both reach the broker, the
child's never does.

This is the failure the graph-start gate was added to prevent — `set()` returning
`Ok` into a buffer nobody drains — reappearing on the other side of a `fork`. It
is worse across a C ABI than in Rust, because a C++ daemon that double-forks, or
a supervisor that forks per job, is an ordinary shape rather than an exotic one.

**Where:** `aimdb-sync`. The handle already knows when it is unusable
(`RuntimeShutdown` when the weak upgrade fails); it needs to know this case too.
The cheap version is a `pid` stamped at attach and compared on the `set`/`get`
path, so a child fails with a distinct error instead of succeeding. The complete
version is a `pthread_atfork` child handler that marks every live handle
poisoned.

**Why it cannot be fixed in the FFI layer:** either mechanism is process-global.
An FFI shim registering `pthread_atfork` on behalf of an application that did
not ask is the same category of trespass as installing a logging subscriber —
the rule the Python door established and this one kept.

**Acceptance:** a child of `fork` gets a distinct error from `set`/`get`, in a
test that forks.

### CR-2 — No unbounded wait on the attach path

**Read** — `aimdb-sync/src/handle.rs:209-215`. `attach` spins on

```rust
let runtime_handle = loop {
    let handle_opt = runtime_handle_result.lock().unwrap().clone();
    if let Some(handle) = handle_opt { break handle; }
    thread::sleep(Duration::from_millis(1));
};
```

and the runtime thread that is supposed to fill that slot has an early return
(`handle.rs:186-191`): if `tokio::runtime::Runtime::new()` fails, it logs and
exits, and nothing is ever stored. `attach` then spins forever.

Nothing above can time out around it. `StationHandle::open` has a
`GRAPH_START_TIMEOUT`, but it is armed *after* `builder.attach()` returns — so
the hang is upstream of every timeout the station crate owns, and upstream of
anything the FFI layer can do. For a C++ caller it is `ws_station_open_profile`
never returning, with no signal, no error and no way to cancel.

**Where:** `aimdb-sync`. A `sync_channel(1)` carrying `Result<Handle, Error>`,
sent on both paths, replaces both the sleep-loop and the silent failure.

**Acceptance:** `attach` returns `Err(SyncError::AttachFailed)` when the runtime
thread cannot start, with no code path that can wait unboundedly.

### CR-3 — A bounded shutdown must be truthful about what it left running

**Read** — `aimdb-sync/src/handle.rs:341-390`. On timeout, `detach_timeout`
returns `Err(DetachFailed)` while the runtime thread is still running, and the
helper thread it spawned to join that thread is never reclaimed — it stays
blocked in `join()` for the life of the process. The caller is given no way to
wait longer, and no way to learn later that shutdown finished.

Across a C ABI this is not a leak but a lifetime question: `ws_station_free`
calls shutdown and then drops the box. If shutdown reports failure while the
runtime thread is still alive, the FFI layer has no correct next move — freeing
races the thread, and not freeing leaks the station. Today it frees, on the
strength of aimdb's own weak-reference discipline; that is an assumption the
core should either confirm in writing or remove.

Two smaller things in the same function: the wait polls on a 10 ms sleep rather
than a condvar, so every shutdown pays up to 10 ms it does not need (visible as
the 10 ms floor in every `close()` measurement in the spike), and `Drop` falls
back to a 5-second emergency shutdown with a warning — which for C++ means a
destructor that can block five seconds and log from inside `~Station`.

**Where:** `aimdb-sync`.

**Acceptance:** a timed-out detach leaves no unreclaimable thread and documents
exactly what the caller may do with the handle afterwards.

### CR-4 — A panic-freedom contract on the blocking surface

**Measured** — round *"nothing unwinds across the boundary"*. The FFI layer
catches: `ws_debug_panic` returns `WS_ERR_PANIC` and the process survives. But
two things the catch cannot do put the requirement upstream.

It does not stop the message: a panic writes its text and backtrace to **fd 2**
(1,732 bytes, measured) past the installed sink, because Rust's panic hook is
process-global. And it is compiled out by `panic = "abort"`, which a consumer's
profile may set without knowing this library is in the graph — turning every
catch into an abort of the whole C++ process.

Installing a panic hook in the FFI layer would be the same trespass
`init_tracing` was in the Python module — an extension deciding where the
application's diagnostics go. So the fix is upstream and it is a discipline, not
a feature: **the blocking surface must not panic.**

Concretely, the two reachable `Mutex::lock().unwrap()` sites at
`aimdb-sync/src/handle.rs:195` and `:211` should recover from poisoning the way
`weather-station` already does (`unwrap_or_else(|p| p.into_inner())`), and the
crate should carry a `#![deny(clippy::unwrap_used, clippy::expect_used,
clippy::panic)]` on the non-test surface so the property is checked rather than
remembered.

**Where:** `aimdb-sync`, and the same rule stated for `aimdb-core`'s public API.

**Acceptance:** the sync surface is lint-clean under those denies, and the docs
say a panic is a bug rather than an error channel.

### CR-5 — A stable error classification in the core, not only in the station crate

**Measured, by inheritance.** The C++ door maps failures to exceptions through
`StationError::kind()` — three actions, exhaustive inside the crate that owns
the enum, so a variant added later is a compile error there rather than a silent
reclassification at the boundary. That fix, made for the Python door, is what
made this door's exception hierarchy possible at all.

The core has no equivalent. `DbError` (`aimdb-core/src/error.rs:103`) has ~20
variants and no classifier; `SyncError` (`aimdb-sync/src/error.rs:12`) has six.
Neither is `#[non_exhaustive]`, so today every added variant is a breaking
change, and any FFI layer built on aimdb *directly* — rather than on a station
crate that has already done this work — must either match twenty variants and
redo it each release, or collapse everything to one opaque code.

A C caller's `switch` has a `default` arm, exactly as a Rust wildcard does. The
classification is what keeps that arm from quietly swallowing new failures.

**Where:** `aimdb-core` and `aimdb-sync`.

**Acceptance:** `DbError::kind()` and `SyncError::kind()` returning small
`Copy` enums, both error types `#[non_exhaustive]`, both `kind()` matches
exhaustive inside their own crate. Decide this before the release: adding
`#[non_exhaustive]` afterwards is itself breaking.

---

## B. Needed before a C ABI can expose more than publishing

The door is publish-only, as the Python one was. These are what the consumer
half needs, and they are listed now because the first is an API-shape decision
that gets more expensive after a registry release.

### CR-6 — `SyncConsumer` must be usable through a shared reference

**Read** — `aimdb-sync/src/consumer.rs:105,148,189,239,290`. Every consumer
method takes `&mut self`, while `SyncProducer::set` takes `&self`.

That asymmetry is exactly the shape that broke the Python door: `close(self)`
could not be bound because a `#[pymethods]` method never receives `self` by
value, and the workaround — `&mut self` — collided at run time with a publish
already in flight. The fix was to make the operation take `&self`. A
`ws_consumer*` reproduces the problem one level lower and with no diagnostic:
two threads calling a `&mut self` method through one raw pointer is aliasing
undefined behaviour, and C has neither pyo3's borrow flag nor Rust's borrow
checker to notice.

The FFI layer can hide it behind a mutex, but that is the wrong place: it
serialises every consumer in every language binding to work around a signature.

**Where:** `aimdb-sync`. Interior mutability in `SyncConsumer` so `get`,
`get_with_timeout`, `try_get`, `get_latest` and `get_latest_with_timeout` take
`&self`, matching the producer.

**Acceptance:** `SyncConsumer` is `Sync` *and* usable from several threads
through one shared reference, pinned by a test that does it.

### CR-7 — A delivery signal, or a documented "there is none"

**Measured** — round *"a reading published immediately before close can be
lost"*: 5/8 temperatures and 2/8 humidities from eight publish-then-close cycles
with no grace; 8/8 of each with 20 ms. The numbers move between runs and
machines, which is the point — nothing between the buffer and the socket reports
what was written.

Both doors reached the same conclusion, that a station publishing on a cadence
loses only a reading nobody would have read, and both recorded it rather than
adding a grace period. It stays on this list because publish-once-and-exit is a
shape a C or C++ station is *more* likely to have than a Python one — a cron job
or a one-shot sensor read is idiomatic there — and the answer, when it is
needed, is a delivery signal over a standard MQTT ACK topic rather than a
shutdown that waits.

**Where:** `weather-station` if it stays mesh-specific; `aimdb-core` if a
general "flushed to the link" signal is wanted. Not required for this release,
but the decision is.

---

## C. Contracts the core should publish

Not code, but release artifacts: the C++ door depends on all of these and none
of them is currently written down anywhere a consumer would find it.

### CR-8 — A threading and reentrancy contract, per method

Rust has no way to express "safe to call from the runtime thread". The spike
proves it holds today — a sink that calls `ws_station_is_closed` and
`ws_station_slot` from inside the logging path, on aimdb's runtime thread,
including during that station's own shutdown, returns in 10 ms — but nothing
stops a future change from making a getter take the mutex `shutdown` holds. In
Python that regression deadlocks under the GIL; in C++ it deadlocks under
whatever the callback locks. Neither is a compile error.

**Acceptance:** every public method on `AimDbHandle`, `SyncProducer` and
`SyncConsumer` documents whether it may block, whether it may be called
concurrently through a shared reference, and whether it may be called from the
runtime thread — with the last one pinned by a test that calls it from there.

### CR-9 — "No aimdb library installs a process-global" as a checked rule

`aimdb-core`'s `log_*` macros are a feature-gated facade and no aimdb library
installs a subscriber — verified in this checkout; the only crate that ever
broke the rule was the Python spike module itself, and it was fixed. Now that
two FFI layers exist, the rule is load-bearing enough to check rather than
remember, and it has more members than logging: a subscriber, a panic hook, a
signal handler, `pthread_atfork`, `set_var`.

CR-1 is the exception that proves it, and the reason it is a *core* requirement:
the fork handler is process-global, so it may only be registered by the crate
that owns the runtime thread — never by an FFI shim on the application's behalf.

**Acceptance:** a CI grep or a `deny.toml`-style rule over the library crates.

### CR-10 — Pin the shutdown contract in `aimdb-sync`, not only in `weather-station`

Four properties make an FFI layer possible, and all four currently live in
`weather-station::StationHandle` rather than in the crate whose thread they are
about: shutdown takes `&self`; it is idempotent; "closed" is observable through
an atomic rather than the lock shutdown holds; and the wait is bounded.

Every language binding needs all four — the C ABI's free function no more
receives `self` by value than a `#[pymethods]` method does — so each one that
binds `AimDbHandle` directly rather than a station crate will rediscover them.

**Acceptance:** `AimDbHandle` grows `shutdown(&self)` / `is_closed()` with the
same semantics, and `aimdb-sync` carries the tests: shutdown under concurrent
producers, `is_closed` from another thread mid-shutdown, double shutdown.

---

## D. Build surface

### CR-11 — Make the TLS backend selectable

**Measured.** The cdylib links `libssl.so.3` and `libcrypto.so.3`; a static
consumer's link fails without `-lssl -lcrypto` (rustc's own
`--print native-static-libs` names them). It arrives through `rumqttc`'s
`use-native-tls`, which `weather-station`'s `preflight` feature pulls in for a
single CONNECT.

For a Rust binary that is a build detail. For a shared library dropped into
somebody's C++ application it is a system-OpenSSL ABI constraint on every
consuming build, and a second OpenSSL in a process that very likely already has
one — a well-known way to crash at TLS-handshake time rather than at link time.

**Where:** `aimdb-mqtt-connector` (and `weather-station`'s pre-flight, which
should follow it). A `rustls` feature alongside the native-TLS one removes the
system dependency entirely for consumers that want that.

**Acceptance:** an FFI-facing build with no `libssl`/`libcrypto` in `ldd`.

### CR-12 — A log sink that does not require `tracing-subscriber`

Both doors had to add `tracing` *and* `tracing-subscriber` to their manifests to
write a `Layer`, even though neither adds a crate to the build graph. That is
tolerable for two spikes and wrong as a long-term shape: an FFI layer wants to
hand aimdb a function pointer, not implement a `tracing` trait, and every
non-Rust consumer will want the same thing.

**Where:** `aimdb-core`, alongside the `log_*` facade — a
`set_log_sink(fn(level, target, message))`-shaped hook, with the `tracing`
bridge kept as the default implementation for Rust consumers.

**Acceptance:** an FFI layer can route aimdb's reporting with no
`tracing-subscriber` dependency.

---

## What already holds, and should keep holding

Listed because a regression in any of them breaks both doors at once, and
because they are the answer to "what did the Python round already buy us".

| Property | Owner | Evidence |
|---|---|---|
| `shutdown(&self)`, idempotent, safe during a publish | `weather-station` | close in 10 ms with 4 threads publishing |
| `is_closed()` reads an atomic, never the shutdown lock | `weather-station` | reentered from the runtime thread during that station's own shutdown |
| `StationError::kind()` classifies exhaustively inside its own crate | `weather-station` | three exception classes, no wildcard at the boundary |
| The handle is `Send + Sync` | `aimdb-sync` → `weather-station` | 200 publishes on 4 threads through one `const ws_station*` |
| No aimdb library installs a logging subscriber | `aimdb-core` | the sink is the caller's function pointer |
| The graph-start gate closes the startup race for a foreign caller | `weather-station` | 8/8 first readings arrive |
| Producers hold a `Weak`, so a publish never contends with shutdown | `aimdb-sync` | close never queues behind an in-flight publish |
| A blocking call parks its own thread and no other | `aimdb-sync` | 492 ticks on a second thread across a 5 s blocked open |
