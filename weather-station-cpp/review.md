# Review: the two busy-waits in `aimdb-sync`, and what tracing them turned up

Follows `REQUIREMENTS.md`. That document listed CR-2 (an unbounded spin on the
attach path) and CR-3 (a 10 ms poll on the detach path) as things read out of
the source. This one is what happened when they were actually fixed, plus two
corrections and one escalation that fell out of doing it.

Patches against the sibling aimdb checkout are in `patches/`. They are not
applied to any aimdb branch — this repository cannot push there.

---

## 1. Both busy-waits are gone, and neither needed a new idea

### `AimDbHandle::new` — the unbounded spin

```rust
let runtime_handle = loop {
    let handle_opt = runtime_handle_result.lock().unwrap().clone();
    if let Some(handle) = handle_opt { break handle; }
    thread::sleep(Duration::from_millis(1));
};
```

Replaced by a channel and one receive. The mechanism that matters is not the
removal of the sleep, it is what a closed channel means: the runtime thread's
early return drops `handle_tx`, a dropped sender closes the channel, and
`blocking_recv()` yields `None`. A runtime that fails to start becomes
`SyncError::AttachFailed` instead of a caller that never returns.

**The correct implementation was already sixty lines up.** `new_from_builder`
has used exactly this shape since it was written. Nothing had to be designed;
the older constructor simply never picked it up. See §3.

The channel carries a `Result`, not a bare handle:

```rust
/// What the runtime thread reports back while starting up: the thing itself,
/// or why it could not be produced.
type Startup<T> = Result<T, String>;

fn recv_startup<T>(rx: &mut mpsc::Receiver<Startup<T>>, what: &str) -> SyncResult<T> {
    match rx.blocking_recv() {
        Some(Ok(value)) => Ok(value),
        Some(Err(cause)) => Err(SyncError::AttachFailed { message: cause }),
        None => Err(SyncError::AttachFailed {
            message: format!("runtime thread stopped before sending the {}", what),
        }),
    }
}
```

Three outcomes, three arms, no fourth state. `None` is the one that used to be a
hang. `Err` is an addition rather than a consequence, and it is what an FFI
consumer actually needs: without it a startup failure arrives as "runtime thread
failed to send handle" while the real `io::Error` goes only to a log sink the
caller may never have installed. The thread now reports its reason before dying:

```rust
Err(e) => {
    let cause = format!("Failed to create Tokio runtime: {}", e);
    log_error!("{}", cause);
    let _ = handle_tx.blocking_send(Err(cause));
    return;
}
```

`new_from_builder` gets the same treatment on both of its channels, which also
recovers the reason a *database build* failed — today that reason exists only in
the log. Measured end to end by forcing a build failure and catching at the C++
boundary:

```
before: Failed to attach database: Runtime thread failed to build database
after:  Failed to attach database: Failed to build database: missing parameter 'broker.url'
```

### `detach_internal` — the 10 ms poll

```rust
loop {
    if handle_thread.is_finished() { break; }
    if start.elapsed() > duration { return Err(...); }
    thread::sleep(Duration::from_millis(10));
}
```

`JoinHandle` has no timed join, so the helper thread stays — but it now reports
through a `std::sync::mpsc::channel` and the caller uses `recv_timeout`. The
wait parks instead of polling, and the timeout is exact rather than rounded up
to the next tick.

**Measured effect**, from the same spike rounds before and after:

| Round | Before | After |
|---|---|---|
| `close()` succeeds while sensor threads publish | 10 ms | 1 ms |
| `close()`, which joins the runtime thread | 10 ms | 0 ms |
| `close()` while the sink reenters that station's own getters | 10 ms | 0 ms |
| a second station's events reenter the first station's getters | 12 ms | 2 ms |

That 10 ms was not the runtime thread taking 10 ms to stop. It was the poll
interval, paid by every shutdown in every language binding.

### State after both patches

A sweep of `aimdb-sync/src/` for `thread::sleep`, `is_finished`, `yield_now` and
`spin` returns one comment and no code. The crate no longer busy-waits anywhere.

Verified: `aimdb-sync` compiles and is `cargo fmt` clean, `cargo test -p
weather-station --features sync` passes (16 / 2 ignored / 4), and `make
spike-cpp` is all-green with the timings above.

One constraint is unchanged and worth stating in the docs: `blocking_recv` and
`recv_timeout` both park the calling thread, so neither constructor may be
called from inside a Tokio runtime. `new_from_builder` already carried that;
`new` now carries it too, and `StationHandle` documents the same rule one layer
up as "not reentrant into a runtime".

### What the detach patch does *not* fix

The helper thread still cannot be reclaimed on timeout — you cannot cancel a
`join()`. It exits on its own whenever the runtime thread finally stops, and its
send fails harmlessly into a dropped receiver. The rest of CR-3 stands: a
timed-out detach still returns while the runtime thread is alive, and the
contract for what a caller may then do with the handle is still unwritten.

---

## 2. Correction: CR-2 was on a path the C++ door does not take

`REQUIREMENTS.md` says of the spin: *"For a C++ caller it is
`ws_station_open_profile` never returning."* That is wrong.

There are two `attach()` entry points:

| Entry point | Constructor | Shape |
|---|---|---|
| `AimDbBuilderSyncExt::attach` on `AimDbBuilder` | `new_from_builder` | channel + `blocking_recv` — never spun |
| `AimDbSyncExt::attach` on a built `AimDb` | `new` | the spin |

`weather-station` calls `builder.attach()`, so it has always taken the clean
path. The spin sits on the other public entry point — reachable by an FFI layer
built directly on aimdb, but not by this door.

CR-2 is still a real defect on a public API, and it is now fixed. It was not the
severity the document gave it, and the claim that it could hang the C++ station
was not checked before it was written down.

---

## 3. Why the second constructor exists

Asked because "delete it" is a better fix than "repair it" when a function has
no users.

**It is the original design, not a duplicate.** Design doc `007-M2-sync-api.md`
documents `db.attach()` as *the* entry point — at §Thread Lifecycle ("Main
thread calls `db.attach()`"), in the architecture example, and in the complete
example in its appendix. `AimDbBuilderSyncExt::attach` came later, once
`AimDbBuilder::build()` became async and therefore had to run *inside* the
runtime thread rather than before it. That is why the newer path also carries
the database back over a channel: it has one more thing to hand across.

**It has no users left.** Every call site in the aimdb repository — integration
tests, characterization tests, doc examples on `SyncProducer`, `SyncConsumer`
and the crate root — goes through `builder.attach()`. The only occurrences of
`AimDbSyncExt` are its own definition, its own doc example, its re-export in
`lib.rs`, and the three design-doc passages above. Nothing in this repository
uses it either.

So the busy-wait was not a shortcut anyone chose. It is the residue of an entry
point that was correct when written, superseded by one built to a better shape,
and never revisited because nothing exercised it.

**Recommendation.** Keep it, now that it is fixed — it is the only way to attach
a database built somewhere else, which is a reasonable thing for an embedder to
want, and it is cheap once it shares the newer path's shape. But it needs a test
that actually calls it, because a public entry point with no in-tree caller is
how this happened. If the team would rather not carry it, deprecating it in
0.6.0 is the cheaper option, and 0.x makes removal in 0.7.0 routine.

---

## 4. CR-6: the diagnosis was right, the remedy and the severity were wrong

Two passes of this review said different things about `SyncConsumer`, and both
were wrong in the same direction. `REQUIREMENTS.md` filed it as "an API shape to
reconsider". The first pass of this document escalated it to "a breaking
regression, the one release-gating finding". Measuring it says: neither.

Evidence is `probes/cr6-consumer-shape`, which exercises aimdb's API directly —
the mesh spike cannot ask these questions, because `StationHandle` is
publish-only.

### What is true

`SyncConsumer` is `Send + !Sync`, has no `Clone`, and its methods take
`&mut self`. Confirmed by the compiler:

```
error[E0277]: `(dyn BufferReader<f32> + Send + 'static)` cannot be shared between threads safely
note: required because it appears within the type `Reader<f32>`
note: required because it appears within the type `SyncConsumer<f32>`
```

And the crate's own changelog files it under **Changed (breaking)** for the
unreleased 0.6.0, under issue #200. That much stands.

### What the remedy should have been checked against

`&mut self` is not an oversight, it is the honest signature. A reader is a
*cursor* — `BufferReader::poll_recv(&mut self)` advances it — so reading
mutates. The question was never "can this be `&self`" but "what does a consumer
represent", and the answer makes the FFI shape fall out.

**A consumer is a subscription, and the handle hands out as many as you like.**
`AimDbHandle::consumer()` takes `&self`, the handle is `Sync`, and each call is
a fresh `subscribe_boxed()` with its own cursor:

```
1. N threads, one consumer each, created concurrently:
   thread 0 saw [0, 1, 2, 3, 4]
   thread 1 saw [0, 1, 2, 3, 4]
   thread 2 saw [0, 1, 2, 3, 4]
   thread 3 saw [0, 1, 2, 3, 4]
   => every consumer has its own cursor: YES

2. cost of handle.consumer(): 891ns per call over 1000 calls
```

So the consumer half of a C ABI needs no core change at all:
`ws_consumer_open(station, key)` per thread, each `ws_consumer*` single-owner —
the same ownership rule the header already states for `ws_station_free`, and one
the C++ layer can turn back into a compile error with a move-only type.

**Blocking consumers do not cost runtime workers.** `Waiter::block_on` is
`Handle::block_on`, which drives the future on the *calling* thread. Sixteen
threads parked in `get()` at once, all served:

```
4. 16 threads blocked in get() at once: 16/16 served
```

That is the C++ hub shape, and it was the thing actually worth worrying about.
(Same reentrancy rule as the constructors in §1: `Handle::block_on` panics
inside a Tokio runtime.)

**The shared case already has a one-line answer.** `Mutex<T>` is `Sync` whenever
`T: Send`, so a caller who genuinely wants one stream split across workers
writes `Arc<Mutex<SyncConsumer<T>>>`, and it compiles today:

```
5. Arc<Mutex<SyncConsumer>> across 3 threads: each value went to exactly one worker: [100, 101, 102]
```

### Why the proposed fix would have made things worse

Putting that `Mutex` *inside* `SyncConsumer` to recover `&self` — which is what
CR-6 asked for — would impose split semantics on every caller, including the
majority who want fan-out. It would also break `try_get`'s contract: a
non-blocking call would have to wait on a mutex held by a blocking `get()`,
which is exactly the "never blocks" promise the method exists to make.

### What is actually left

Documentation and a migration note, not an API change.

- **Say what a consumer is.** "A subscription with its own cursor. Create one
  per thread. Wrap it in a `Mutex` only if you want several threads to split one
  stream." None of that is written down today, and it is the whole design.
- **The changelog entry is accurate but reads as a loss.** "now takes
  `&mut self` […] no longer implements `Clone` or `Sync`" tells a 0.5.0 user
  what was removed and not what replaces it. It should name the replacement.
- **One real migration hazard.** Cloning a consumer in 0.5.0 shared one stream
  (split). Calling `handle.consumer()` twice in 0.6.0 gives two streams
  (fan-out). A user who mechanically replaces `clone()` with `consumer()` gets
  *different behaviour*, silently — each worker starts seeing every value
  instead of its share. That deserves a sentence in the changelog, because it is
  the one way this change can break a working program with no compile error.

### On "release-gating"

It is not. Nothing about the C++ consumer path is blocked, the capability that
matters is available today, and the gap is prose. Retracting the escalation from
the previous pass of this review: I asserted a blocker from a changelog entry
and a compiler error without asking what the API was for.

## 5. CR-5: one classification for the whole stack, and what it costs

Implemented, in `patches/aimdb-error-classification.patch`. This is the one
finding with a deadline attached, so the point of doing it was to find out what
it actually costs rather than to argue about it.

### The shape

`DbErrorKind` — eight kinds, each a different thing the caller does:

| Kind | The caller… | From |
|---|---|---|
| `Retry` | tries again | `BufferEmpty`, `BufferFull` |
| `Lagged` | notes the gap and continues | `BufferLagged` |
| `Closed` | stops | `BufferClosed` |
| `Transport` | retries, or fixes the deployment | `ConnectionFailed`, `Io`, `IoWithContext` |
| `Data` | fixes the schema or the sender | `Json`, `JsonWithContext` |
| `Configuration` | fixes the graph before starting | `MissingConfiguration`, `InvalidConfiguration`, `CyclicDependency`, `TransformInputNotFound` |
| `Usage` | fixes this call site | `RecordNotFound`, `RecordKeyNotFound`, `InvalidRecordId`, `TypeMismatch`, `InvalidOperation`, `PermissionDenied` |
| `Internal` | reports a bug | `RuntimeError`, `Internal` |

Eight rather than `StationErrorKind`'s three because a database has more than
three answers, and the test is not "how few" but **does each kind name a
different action**. `Lagged` is separate from `Retry` because the caller lost
data and may want to say so; `Closed` is separate because retrying cannot help.

`SyncError::kind()` returns the *same* enum rather than one of its own, so an
FFI layer has one switch for the whole stack: `Db(e)` delegates, timeouts are
`Retry`, `RuntimeShutdown` is `Closed`. A buffer that is merely empty classifies
identically whether it is reached through the facade or through `aimdb-core`.

This follows a house style that already exists — `RpcError`, `CodecError` and
`AuthError` in `session/mod.rs` are all `#[non_exhaustive]` with small,
action-shaped variant sets. `DbError` and `SyncError` are the outliers, not the
precedent.

### Both halves of the property, verified

**Inside the crate, a new variant is a compile error.** Adding one to `DbError`:

```
error[E0004]: non-exhaustive patterns: `&DbError::ProbeVariantAddedLater` not covered
   --> aimdb-core/src/error.rs:324:15  (in `DbError::kind`)
```

Someone has to decide which action it belongs to, in the file that owns it.

**Outside the crate, a wildcard is mandatory.** A downstream match naming all
21 variants explicitly, with no wildcard:

```
error[E0004]: non-exhaustive patterns: `&_` not covered
    = note: `BufferError` is marked as non-exhaustive, so a wildcard `_` is necessary
```

Together those two are the whole mechanism: reclassification happens once,
deliberately, where the enum lives — instead of silently, at every boundary that
had to write a wildcard.

### What it costs: nothing that compiles today

The blast radius was the open question, and it is smaller than the variant count
suggests. `#[non_exhaustive]` on an enum blocks exhaustive *matching* downstream;
it does not block *construction*, which is what most of the 19 files outside
`aimdb-core` that mention `DbError::` are doing.

Of the 24 real match arms across the workspace, every one is partial — code
matches `BufferEmpty`, `BufferLagged` or `BufferClosed` and falls through. Nobody
writes an exhaustive match over 21 variants. Adding both attributes compiled
`aimdb-core`, `aimdb-sync`, `aimdb-tokio-adapter`, `aimdb-mqtt-connector`,
`aimdb-data-contracts` and the whole mesh workspace with no change to any of
them, and `make spike-cpp` stayed green.

Not verified: `aimdb-embassy-adapter` and `aimdb-wasm-adapter`, which the mesh
workspace does not build. Their arms (`buffer.rs` in both) are partial in the
same way, so the expectation is the same, but that is a read rather than a
build.

### The judgment calls, flagged rather than buried

- **`PermissionDenied` sits in `Usage`.** Arguable — it could be its own kind if
  the remote-access work ever wants to distinguish "you may not" from "that is
  not a thing".
- **`BufferFull` sits in `Retry` alongside `BufferEmpty`.** They are opposite
  conditions with the same answer. If backpressure ever needs a different
  response from starvation, that is the first kind to split.
- **`TransformInputNotFound` sits in `Configuration`, not `Usage`.** It is a
  graph wiring mistake found at build time, not a bad call.

### One thing this exposed, which is CR-2's to finish

`SyncError::AttachFailed { message: String }` carries a `String`, so it
classifies as `Internal` even when the underlying cause was a configuration
mistake — and after the CR-2 patch that message now often *is* a configuration
mistake, spelled out. The kind is thrown away one layer below where it would be
useful.

The fix is small and lands in the code CR-2 already touched: make the startup
channel carry `DbError` instead of `String`, so `AttachFailed` can keep the
cause's kind rather than flattening it. Left out of this patch because it
changes a `SyncError` variant's shape, which is a decision rather than a
cleanup — and `aimdb-sync` being 0.x makes it cheap whenever it is taken.

### On the deadline

`aimdb-sync` is 0.6.0 and unreleased, so `#[non_exhaustive]` there is free today
and a routine 0.7.0 minor bump later. `aimdb-core` is 1.2.0 with 1.0.0 already
shipped, so the attribute is a 2.0 either way — this release does not open or
close that window, it only decides whether the window opens with `kind()`
already in place. `kind()` alone is purely additive and can ship in 1.2.0
regardless of what is decided about the attribute.

## 6. CR-1: fork safety, and a second failure the first probe hid

Implemented across `patches/aimdb-sync-fork-safety.patch` and this repository.
The only finding on the list that the spike *measured* as broken, and the only
one where the fix had to be a process-global — which is why it belonged in the
core rather than in the FFI layer.

### There were two failures, not one

The original round showed the silent drop: the child is told the station is
open, `publish` returns `WS_OK`, the reading never reaches the broker. That
round ended the child with `_exit(0)`, so it never ran a destructor. Running one
finds the second failure:

```
child: about to let the destructor run
thread '<unnamed>' panicked at library/std/src/thread/lifecycle.rs:247:
threads should not terminate unexpectedly
   5: std::thread::lifecycle::JoinInner<T>::join
   7: aimdb_sync::handle::AimDbHandle::detach_internal
```

A forked child holds a `JoinHandle` for a thread that does not exist in this
process, and joining it panics inside `std`. The panic lands on the helper
thread rather than unwinding into C++, so `~Station` returns — but a Rust
backtrace goes to fd 2 from inside a destructor, which is the CR-4 pattern
again, reached by a different route. Present before this review's changes as
well: the pre-CR-3 code joined on a helper thread too.

### Detection: measured, not assumed

The obvious check is a pid comparison. It is far too expensive to sit where it
has to sit:

```
try_set()            121ns
std::process::id()   321ns   (265.3% of a publish)
relaxed atomic load  0ns     (0.00% of a publish)
```

Reading the pid per publish would cost more than twice the work it guards. So
detection is a `pthread_atfork` child handler, and the check on the hot path is
a relaxed atomic load. Re-measured after the change: `try_set()` at 107 ns —
the guard does not show up.

### A generation counter, not a flag

A `bool` would poison the child permanently, including for a database the
*child itself* attaches afterwards — a supervisor that forks per job and then
does its own work would find the API dead for no reason. `GENERATION` is an
`AtomicU64` the child handler increments; a handle, producer or consumer records
it at construction and is stale when the two differ. Anything made *after* the
fork is fine.

The guard sits before the `Weak` upgrade, deliberately: a forked child's upgrade
*succeeds*, because the `Arc` was copied with the address space. That is exactly
why the buffer accepts a value nobody will read.

### What it changed, end to end

The spike's three notes are now four checks:

```
after fork(), the child has no runtime thread
  ok    the parent keeps publishing across the fork — 2 of the parent's 2 readings arrived
  ok    a forked child is told the station is closed — is_closed() reported closed
  ok    a forked child's publish is refused, not silently dropped — threw
  ok    no phantom reading reached the broker
```

and the destructor probe returns without a panic.

Reached by four changes: `SyncProducer` and `SyncConsumer` refuse a stale
generation with a new `SyncError::ForkedChild`; `AimDbHandle::producer` and
`consumer` refuse to hand out more; `detach_internal` and `Drop` release the
`JoinHandle` instead of joining a thread this process does not have; and
`StationHandle::is_closed` — in this repository — reports closed in a child,
since a station that cannot publish is closed in every sense a caller cares
about.

### Two things worth naming

**CR-5 paid for itself immediately.** `SyncError::ForkedChild` is a new variant,
which before this review would have been a breaking change. `SyncError` is
`#[non_exhaustive]` as of §5, so it is additive — downstream matches already
carry the wildcard that makes it safe. The classification work turned the next
fix from a version bump into a patch.

**`fork::generation` and `fork::forked_since` are public**, which was not the
plan. `StationHandle::is_closed` must not take the mutex `shutdown` holds — the
lock-ordering rule the Python door established — so it cannot ask the
`AimDbHandle` whether the process forked. It records the generation itself and
compares. Any FFI layer built directly on `aimdb-sync` will have the same
problem for the same reason, so the two query functions are part of the surface
rather than an internal detail.

### The registration caveat, stated rather than skipped

§7's rule (CR-9) says no aimdb library installs a process-global. This installs one.
It is the exception the rule anticipated, on two conditions that are both met:
only the crate owning the runtime thread can know the thread is gone, so nobody
above can perform this check; and the handler is registered lazily, on the first
`attach`, so a program that never uses the sync facade never gets one. The
handler does a single relaxed `fetch_add`, which is permitted in a fork handler.

### Left open

Which layer answers first is a diagnostic question the fix does not settle. The
station's own closed-check wins over aimdb's more specific message, so a C++
caller reads "this station is closed" rather than "created before a fork()".
Recorded as a `note` in the spike rather than fixed, because closing that gap
means teaching a third layer about forks to improve one string.

## 7. CR-11: the TLS backend, and what selecting it actually costs

Implemented across `patches/aimdb-mqtt-connector-tls-backend.patch` and this
repository. The finding was measured from the start — `ldd` on the cdylib — so
the work here was to find out whether the fix is as cheap as it looked. It is
not quite, and there is a size bill nobody had counted.

### It was a deliberate choice, not an oversight

rumqttc's own default is `use-rustls`. Both `aimdb-mqtt-connector` and
`weather-station` set `default-features = false` and then explicitly opt into
`use-native-tls` — the system OpenSSL was selected *over* the pure-Rust default.

Worth noting because the embedded half already went the other way: design 044's
Embassy client uses `embedded-tls` with `rustpki`, `rsa` and `p384`, a pure-Rust
stack with no system dependency at all. The host path is the outlier.

### The measurement

Before, on the cdylib:

```
libssl.so.3    => /lib/x86_64-linux-gnu/libssl.so.3
libcrypto.so.3 => /lib/x86_64-linux-gnu/libcrypto.so.3
native-static-libs: -lssl -lcrypto -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
```

After, with `rustls`:

```
libgcc_s.so.1  libm.so.6  libc.so.6      (and the loader)
native-static-libs: -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
```

For a shared library dropped into somebody else's C++ application that is the
whole point: no system-OpenSSL ABI constraint on every consuming build, and no
second OpenSSL in a process that very likely already has one.

### The bill

| | native-tls | rustls |
|---|---|---|
| release `.so` | 3.9 MB | 8.1 MB |
| stripped | 2.9 MB | 6.6 MB |
| `ldd` | libssl, libcrypto | — |

rustls more than doubles the artifact, because it links the TLS stack it no
longer borrows. For a library shipped into a foreign process that is usually the
right trade — a version conflict between two OpenSSLs is a crash at handshake
time, and 3.7 MB is not — but it is a real number and it belongs in the
decision rather than in a footnote.

### Three states, not two

`tokio-native-tls`, `tokio-rustls`, or neither. The third is a genuine choice
rather than an oversight: a deployment that speaks `mqtt://` to a broker on a
trusted network links no TLS stack at all, which for a shared library is the
difference between inheriting a system OpenSSL ABI and inheriting nothing.
`mqtts://` then fails at connect time with a message naming the missing feature.

That third state was the one piece of real work. rumqttc gates
`TlsConfiguration` **and** `Transport::Tls` on having a backend, so a build with
neither cannot even name the types — the whole `if scheme == "mqtts"` branch has
to be `#[cfg]`'d, not just its argument. A first attempt that gated only the
configuration did not compile.

### A panic found on the way

The obvious rustls translation is `TlsConfiguration::default()`. It is not
usable here:

```rust
for cert in load_native_certs().expect("could not load platform certs") {
    root_cert_store.add(cert).unwrap();
}
```

Two panics on the connect path, in a crate reached through an FFI boundary where
a panic is undefined behaviour rather than an error. Both backends now build the
configuration explicitly, and a machine with no usable trust roots gets a
message saying so. This is CR-4's rule applied to a dependency rather than to
aimdb's own code — worth noting that "the library must not panic" has to extend
to what the library calls.

### Where the choice lives

In `weather-station`, and passed down. The pre-flight probe and the connector's
data path must not disagree: two backends in one process means two TLS stacks,
and a pre-flight that succeeded where the data path failed would defeat the
point of probing at all. So `weather-station`'s `native-tls` and `rustls`
features each enable both halves, and there is no way to name them separately.

`weather-station`'s default is now `["tokio-runtime", "native-tls"]`, so every
existing station builds exactly as before. `weather-station-cpp` — the crate
that actually ships as a shared library — takes `default-features = false` with
`rustls`.

### Verified

Four combinations build and test: default (native-tls), `rustls`, no backend,
and the whole workspace. `make test` and `make clippy` carry all of them now, in
the two entries added to each — this repository's convention is that the feature
matrix lives in the Makefile, and a TLS backend is exactly the kind of choice
that rots if CI only ever sees one side of it. `make spike-cpp` stays green and
the cdylib's `ldd` stays clean.

## 8. Status of the requirements this review touched

| | Was | Now |
|---|---|---|
| CR-2 unbounded spin on attach | read, severity overstated | **fixed**, patch in `patches/`; scope corrected in §2 |
| startup failures reached the caller without a reason | not filed | **fixed** in the same patch — `Startup<T>` carries the cause into `AttachFailed` |
| CR-3 detach poll + timeout | read | **poll fixed** (10 ms → 0 ms); unreclaimable helper and the post-timeout contract still open |
| CR-4 `unwrap` on a poisoned mutex | two sites | **both gone** — the `Arc<Mutex<Option<Handle>>>` they guarded no longer exists |
| CR-6 `SyncConsumer` shared access | "an API shape to reconsider", then "release-gating" | **neither** — the per-thread consumer works today at 891 ns; what is missing is documentation and a migration note. See §4 |

| CR-5 error classification | filed, not designed | **implemented** — `DbErrorKind` (8 kinds), `SyncError::kind()` delegating to it, both enums `#[non_exhaustive]`; patch in `patches/`. Both halves of the exhaustiveness property verified, no downstream breakage. See §5 |

| CR-1 fork safety | measured, broken | **fixed** — fork generation + `pthread_atfork`, guards on producer, consumer, factories, detach and `Drop`; `StationHandle::is_closed` fork-aware. Four spike checks where there were three notes. See §6 |

| CR-11 TLS backend | measured, unfixed | **fixed** — `tokio-native-tls` / `tokio-rustls` / neither in the connector, mirrored in `weather-station`; the FFI crate builds on rustls and its `ldd` is clean. Costs 3.7 MB. See §7 |

CR-7, CR-8, CR-9, CR-10 and CR-12 are untouched by this review.
