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

## 4. Escalation: CR-6 is a regression landing *in this release*

`REQUIREMENTS.md` filed `SyncConsumer`'s `&mut self` receivers under "before a C
ABI can expose more than publishing", and treated it as a long-standing API
shape to reconsider. Reading `aimdb-sync/CHANGELOG.md` while tracing the above
shows it is neither long-standing nor incidental. Under `[Unreleased]` — the
0.6.0 being prepared — in **Changed (breaking)**:

> **Issue #200:** the internal channel bridge to the `tokio` thread is gone —
> blocking calls now call the runtime directly using the `block_on` seam. API
> implications: […] `SyncConsumer`: `get`, `try_get`, `get_with_timeout`,
> `get_latest`, and `get_latest_with_timeout` now take `&mut self` (was
> `&self`) […] `SyncConsumer` no longer implements `Clone` or `Sync` (still
> `Send`).

Confirmed against the code rather than taken from the changelog. The compiler's
answer, from a compile-time assertion:

```
error[E0277]: `(dyn BufferReader<f32> + Send + 'static)` cannot be shared between threads safely
note: required because it appears within the type `Box<(dyn BufferReader<f32> + Send + 'static)>`
note: required because it appears within the type `Reader<f32>`
note: required because it appears within the type `SyncConsumer<f32>`
```

There is no `Clone` impl on `SyncConsumer`, and its test module asserts `Send`
only, where `SyncProducer`'s asserts `Send + Sync`.

**Why this matters more than the original filing said.** In 0.5.0 a consumer
could be cloned and shared across threads. In 0.6.0 it can be moved to one
thread and used there. Across a C ABI that is the difference between a
`ws_consumer*` any thread may use — the property that makes `ws_station_*` safe
today — and one that is undefined behaviour on second touch, with no borrow
checker and no pyo3 borrow flag to catch it. The producer half kept `&self` and
`Sync`, so the two halves of one binding would need opposite threading rules.

**The root cause is one missing bound, and it is in `aimdb-core`, not
`aimdb-sync`:** `Reader<T>` holds `Box<dyn BufferReader<T> + Send>`. Adding
`+ Sync` there — where the implementations allow it — is what makes the rest
possible; the `&self` receivers then need interior mutability in `SyncConsumer`.

**This is the one finding that answers "is there a blocker" with a yes.** Not
because it cannot be fixed later, but because 0.6.0 is the release that removes
the property. Shipping it and restoring it afterwards means a documented
capability that appears, disappears for one version, and returns — which is
worse for a first FFI consumer than either state on its own.

---

## 5. Status of the requirements this review touched

| | Was | Now |
|---|---|---|
| CR-2 unbounded spin on attach | read, severity overstated | **fixed**, patch in `patches/`; scope corrected in §2 |
| startup failures reached the caller without a reason | not filed | **fixed** in the same patch — `Startup<T>` carries the cause into `AttachFailed` |
| CR-3 detach poll + timeout | read | **poll fixed** (10 ms → 0 ms); unreclaimable helper and the post-timeout contract still open |
| CR-4 `unwrap` on a poisoned mutex | two sites | **both gone** — the `Arc<Mutex<Option<Handle>>>` they guarded no longer exists |
| CR-6 `SyncConsumer` shared access | "an API shape to reconsider" | **a breaking regression in the unreleased 0.6.0**, root-caused to a missing `Sync` bound in `aimdb-core` |

CR-1 (fork), CR-5 (error classification), CR-7 through CR-12 are untouched by
this review.
