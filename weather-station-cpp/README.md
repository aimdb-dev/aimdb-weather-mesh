# weather-station-cpp

The C ABI door onto `StationHandle`, the C++ layer over it — a header rather
than a library, for reasons this document is mostly about — and a station
template on top of both. The pendant of `weather-station-py`, for the language
where no interpreter mediates and the compiler stops helping at the
`extern "C"`.

No soname, no CMake package config, no generated header — those wait for the tag
that ships the library.

## The station

`cpp/station.cpp` is the station itself — the pendant of
`weather-station-openmeteo`, fed by the same API, needing no hardware:

```
make station-cpp CONFIG=station.local.toml
```

It owns its loop and calls `publish_*` when it has a reading, which is what the
blocking door is for. libcurl is the station's dependency, not the library's:
it is where the readings come from, which is the half a station of your own
replaces. The two parsers beside it are deliberately small — a line scanner for
the profile's `[app]` coordinates and a scan for two numbers in a known
response — because a station reading a sensor deletes both. The mesh tables in
that same file are parsed below the ABI, by `ws_station_open_profile`.

`OPEN_METEO_URL` points it at a self-hosted Open-Meteo, or at a fake for
testing. Coordinates come from the profile, then `WEATHER_LAT`/`WEATHER_LON`,
then Vienna; half a pair is an error. SIGINT and SIGTERM set a
`volatile sig_atomic_t` and nothing else: closing from inside a handler would
run aimdb's shutdown on the signal stack.

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

## What a caller has to know

**Nothing may unwind across the boundary.** A Rust panic reaching a C++ frame is
undefined behaviour, so every entry point wraps its body in `catch_unwind` and
reports `WS_ERR_PANIC`; the header's log trampoline is `noexcept` and catches
everything for the same reason in the other direction. A sink that throws is
survivable. Two things that guard cannot do, and both are yours rather than this
layer's: it is compiled out by `panic = "abort"` anywhere in your profile, which
turns every catch into an abort of the whole process, and it does not stop the
message — a panic writes its text and backtrace to **fd 2**, past your installed
sink, because Rust's panic hook is process-global and no library here may
install one.

**Every argument is hostile.** No `Option`, no lifetime, no UTF-8 guarantee.
`NULL` is handled at every entry point. A `const char*` path that is not UTF-8
cannot be represented as a Rust `Path` at all, so `ws_station_open_profile`
refuses it rather than mangling it — on Windows, where the console hands out
UTF-16, a shipped library needs a `_w` entry point or a documented encoding
rule. The C++ header's `std::filesystem::path` constructor converts with
`.string()`, which is where a non-UTF-8 path is lost.

**Ownership is prose in C, a compile error in C++.** `ws_station_free` is the one
entry point that is not thread-safe against the others, because it destroys what
they share. The header makes that a type rule — `Station` is move-only, so two
owners cannot exist — but the C ABI underneath cannot, and a caller using it
directly is on its own.

**A destructor is a shutdown.** `~Station` calls `ws_station_free`, which joins
aimdb's runtime thread. A station held in a static and destroyed after `main`
returns shuts down cleanly, and the destructor never lets an exception out — it
would `std::terminate` during unwinding.

**`close()` is not a flush.** `publish_*` hands the value to the slot's buffer
and returns; the outbound link writes it on the runtime thread. A reading
published immediately before a close may not reach the broker. Stations publish
on a cadence, so what is lost is a reading nobody would have read; a station
that publishes once and exits wants a delivery signal, which does not exist yet.

**Filtering is coarser than `tracing`'s.** `level` and `target=level`,
comma-separated, longest matching prefix first: `info,aimdb_core::builder=debug`.
No span, field or regex directives — a `cdylib` in somebody else's process
should not be installing that process's global subscriber, so
`tracing-subscriber` is not in this build. A Rust consumer keeps `EnvFilter`.

**The log target reaches C unmodified.** `aimdb_core::builder`, `::` intact, for
a `strncmp`. The Python bridge rewrites `::` to `.` because `logging` splits its
hierarchy there; C has no hierarchy, so the two doors deliberately differ.

**The sink cannot be uninstalled.** `log::set_logger` is once per process by
construction, so `callback` and `user_data` must outlive it — which means not
`dlclose`ing this library once `ws_init_logging` has run. On glibc it currently
cannot be unloaded anyway, because Rust's thread-locals give it TLS with
destructors, but that is a platform accident rather than a property to rely on.

**A `fork()`ed child is refused, not silently accepted.** `fork` copies the
address space but not the threads, so a child inherits a station whose graph
nobody pumps. `aimdb-sync` stamps a fork generation and refuses a stale handle,
`is_closed` reports closed, and a publish fails rather than returning `WS_OK`
into a buffer nobody drains. That fix could not live in this layer: the
mechanism is process-global, and an FFI shim registering a `pthread_atfork`
handler on behalf of an application that did not ask is the same trespass as
installing a logging subscriber.

## Notes for whoever ships the library

**Sizes.** Release cdylib 3.9 MB, 2.9 MB stripped. The `staticlib` is 167 MB as
an archive (debug), which is what a static consumer's link step chews through
rather than what it emits.

**No OpenSSL in the link line.** The TLS backend is selectable and this crate
builds on `rustls`, so `ldd` shows only libc, libm and libgcc_s — no
system-OpenSSL ABI constraint on a consumer's build, and no second OpenSSL in a
process that likely already has one. It costs size: 3.9 → 8.1 MB release,
2.9 → 6.6 MB stripped, since rustls links the stack it no longer borrows.

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

**The consumer path, when it lands: a factory, not a handle.** `SyncConsumer`
reads take `&mut self`, unlike `SyncProducer::set(&self)`, so a `ws_consumer*`
shared between threads is aliasing UB with nothing in C to catch it. One call
per consumer, each with its own cursor, and the pointer belongs to the calling
thread. Three contracts the header must state, because no signature can: one
per thread, never share the pointer, never call a blocking read inside an async
runtime.

Deliberately **not** carried: a shared consumer. `aimdb-sync` offers
`Arc<Mutex<SyncConsumer<T>>>` to *split* a stream, and exporting that means
handing a mutex-guarded handle whose purpose is being shared between threads to
a language with no borrow flag. It costs little, because every consumer sees
every value — an application that wants splitting reads the whole stream on one
thread and pushes into a queue it already has.

A caller needing more drops to `aimdb-core` without `aimdb-sync` — available to
a **Rust** consumer, not to someone linking this library. For them this is the
ceiling, and that is the trade.
