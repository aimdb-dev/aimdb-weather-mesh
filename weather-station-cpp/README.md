# weather-station-cpp

The C ABI onto `StationHandle`, a header-only C++ layer over it, and a station
template on both. Not yet shipped: no soname, no CMake package config, no
generated header.

## Run the station

```
make station-cpp CONFIG=station.local.toml
```

`cpp/station.cpp` owns its loop and calls `publish_*` — what the blocking door
is for. Swap `fetch()` for a sensor read and the rest stands; libcurl and
nlohmann/json are the station's dependencies, not the library's — `apt install
libcurl4-openssl-dev nlohmann-json3-dev`. `OPEN_METEO_URL` points at a
self-hosted Open-Meteo or a fake. Coordinates come from the profile's `[app]`,
then `WEATHER_LAT`/`WEATHER_LON`, then Vienna; half a pair is an error. SIGINT
and SIGTERM set a `volatile sig_atomic_t` and nothing else — closing from a
handler would run aimdb's shutdown on the signal stack.

## Two artifacts, and why

Rust cannot export C++: no class, `std::string` or `std::function` has an ABI
stable even between two builds of one compiler. So:

- `include/weather_station.h` — the C ABI. Sixteen `ws_*` symbols, opaque
  pointer, status codes, ownership stated in prose. This is what the library
  exports.
- `include/weather_station.hpp` — RAII, exception hierarchy, move-only
  `Station`, `std::filesystem::path` constructor. **Header-only**, compiled by
  your toolchain, so always ABI-compatible with you.

The rule generalises: the FFI boundary carries the mechanism, the
language-shaped API is written in that language.

The cdylib exports the sixteen symbols and nothing else. The `staticlib` leaks
47,878 text symbols and needs
`-lssl -lcrypto -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc`.

## Rules

| | |
|---|---|
| **Nothing unwinds** | Every entry point catches and returns `WS_ERR_PANIC`; the log trampoline is `noexcept`. A throwing sink is survivable. |
| **`panic = "abort"` defeats that** | Anywhere in your profile, every catch becomes a process abort. |
| **Panics still reach fd 2** | Rust's panic hook is process-global; no library here may install one, so the text bypasses your sink. |
| **Every argument is hostile** | `NULL` handled everywhere. A non-UTF-8 path is refused, not mangled — Windows needs a `_w` entry point or an encoding rule. |
| **`ws_station_free` is the exception** | The one entry point not thread-safe against the others. `Station` is move-only so C++ cannot get this wrong; C callers are on their own. |
| **A destructor is a shutdown** | `~Station` joins aimdb's runtime thread and never lets an exception out. |
| **The sink cannot be uninstalled** | `set_logger` is once per process, so `callback` and `user_data` must outlive it — do not `dlclose` after `ws_init_logging`. |
| **A forked child is refused** | It inherits a station nobody pumps; `is_closed` reports closed and publishing fails rather than returning `WS_OK`. |

## Limits

- **`close()` is not a flush.** A reading published immediately before a close
  may not reach the broker. Stations publish on a cadence, so what is lost is a
  reading nobody would have read — but a station that publishes once and exits
  wants a delivery signal, and there is none yet.
- **Filtering is coarser than `tracing`'s.** `level` and `target=level`,
  comma-separated, longest prefix wins: `info,aimdb_core::builder=debug`. No
  span, field or regex directives — a `cdylib` must not install its host's
  subscriber, so `tracing-subscriber` is not in this build.
- **Log targets keep `::`,** for a `strncmp`. The Python bridge rewrites to `.`
  because `logging` has a hierarchy; C does not.
- **Publish-only.** The consumer path will be a factory: one call per consumer,
  each with its own cursor, the pointer belonging to the calling thread. Never
  a shared consumer — `SyncConsumer` reads take `&mut self`, and a
  mutex-guarded handle in a language with no borrow flag is aliasing UB by
  invitation. Splitting a stream is the application's own business, since every
  consumer sees every value. A caller needing more drops to `aimdb-core`
  directly, which is open to a **Rust** consumer but not to someone linking
  this library.

## Before shipping

- **`-fno-exceptions` does not compile.** The C++ header throws. The C header
  suits that audience; a status-returning C++ variant would be the fix.
- **Generate the declarations, keep the contract.** cbindgen can produce
  `weather_station.h`'s signatures, not its prose — which thread, which
  lifetime, what the callback must not do.
- **`ws_abi_version`.** A header and library from different tags must refuse
  each other at startup rather than disagree about a signature at run time.
- **Size.** Release cdylib 8.1 MB, 6.6 MB stripped. `ldd` shows only libc, libm
  and libgcc_s: the `rustls` backend keeps system OpenSSL out of a consumer's
  link line, and costs about 4 MB to do it.
