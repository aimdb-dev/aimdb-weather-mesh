/* The C ABI of the weather mesh station.
 *
 * Hand-written rather than generated, and that is a finding rather than a
 * shortcut: cbindgen would produce this file from `src/lib.rs`, but every line
 * of prose below — what may be called from which thread, what a pointer's
 * lifetime is, what the callback must not do — is the part a C caller actually
 * needs and the part a generator cannot infer. A shipped library generates the
 * declarations and keeps the contract in a file like this one.
 *
 * Every function is safe to call from any thread on a shared station, with the
 * single exception of ws_station_free.
 */

#ifndef WEATHER_STATION_H
#define WEATHER_STATION_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* --- Status codes ------------------------------------------------------- */

/* The three that are not WS_OK and not a caller mistake mirror
 * StationErrorKind: fix the file, fix the deployment, or neither. A C caller's
 * switch needs a default arm regardless, because StationError is
 * #[non_exhaustive] and this enum is what shields the switch from that. */
enum ws_status {
    WS_OK = 0,
    WS_ERR_PROFILE = 1,          /* edit the profile, or have it re-issued */
    WS_ERR_BROKER = 2,           /* fix the deployment, or re-join */
    WS_ERR_RUNTIME = 3,          /* the station's own machinery or host */
    WS_ERR_CLOSED = 4,           /* the station has been closed */
    WS_ERR_INVALID_ARGUMENT = 5, /* NULL, or a path that is not UTF-8 */
    WS_ERR_PANIC = 6             /* Rust panicked; the station is unusable */
};

/* --- Build identity ------------------------------------------------------ */

/* The ABI this library speaks. Compare against WS_ABI_VERSION at startup: a
 * header and a library from different tags must refuse each other rather than
 * disagree about a signature at run time. */
#define WS_ABI_VERSION 1u
uint32_t ws_abi_version(void);

/* The profile_version this build of the mesh understands. */
uint64_t ws_profile_version(void);

/* --- Errors -------------------------------------------------------------- */

/* The message behind the last failing call *on this thread*, or NULL if the
 * last call succeeded. Owned by the library, valid until the next failing call
 * on the same thread, never freed by the caller.
 *
 * Thread-local because the alternative is two publishing threads overwriting
 * one global. */
const char *ws_last_error(void);

/* --- The station --------------------------------------------------------- */

typedef struct ws_station ws_station;

/* Join the mesh from a station.toml path.
 *
 * Blocks on the broker pre-flight and the graph's first pump — seconds, not
 * milliseconds — and must not be called from a thread that cannot afford to
 * wait. On WS_OK, *out owns a station to be released with ws_station_free.
 * On failure *out is NULL and ws_last_error() explains.
 *
 * `path` must be UTF-8. */
int ws_station_open_profile(const char *path, ws_station **out);

/* Publish a reading, waiting for room in the slot's buffer.
 *
 * Blocking, and safe to call concurrently from as many threads as the caller
 * has sensors: the station is shared, not exclusive. */
int ws_station_publish_temperature(const ws_station *station, float celsius);
int ws_station_publish_humidity(const ws_station *station, float percent);

/* The same, failing rather than waiting when the buffer is full. */
int ws_station_try_publish_temperature(const ws_station *station, float celsius);
int ws_station_try_publish_humidity(const ws_station *station, float percent);

/* The slot number, or 0 for NULL. Still answers after a close: the slot comes
 * from the profile, not the runtime. */
uint16_t ws_station_slot(const ws_station *station);

/* The display name, borrowed. Valid until ws_station_free; never freed by the
 * caller. NULL for a NULL station. */
const char *ws_station_name(const ws_station *station);

/* Whether the station has been closed. True for NULL. Reads an atomic, so it
 * is safe to call while another thread is inside ws_station_close. */
bool ws_station_is_closed(const ws_station *station);

/* Stop the station and shut its runtime thread down.
 *
 * Idempotent, and safe to call while other threads are inside a publish — the
 * shape a signal handler takes. Takes a const pointer on purpose: closing does
 * not need exclusive access to a station a publish is using.
 *
 * A reading published in the last milliseconds before this may not reach the
 * broker. See README.md. */
int ws_station_close(const ws_station *station);

/* Release the station, closing it first if the caller did not. NULL is a
 * no-op, so this can sit unguarded in a destructor.
 *
 * NOT thread-safe against anything else on the same pointer. Every other
 * entry point takes a shared station; this one destroys it. */
void ws_station_free(ws_station *station);

/* --- Logging ------------------------------------------------------------- */

/* Level numbers, chosen to match Python's `logging` so both doors report one
 * event as one number. */
#define WS_LOG_TRACE 5
#define WS_LOG_DEBUG 10
#define WS_LOG_INFO 20
#define WS_LOG_WARN 30
#define WS_LOG_ERROR 40

/* A log sink.
 *
 * `target` is the emitting Rust module path, `::` separators intact.
 * `message` is one line. Both are borrowed for the duration of the call:
 * copy anything you keep.
 *
 * Called from whatever thread emitted the event, which includes aimdb's
 * runtime thread — the one every shutdown waits for. Three rules follow:
 *
 *   1. It must not throw. A C++ exception unwinding into Rust frames is
 *      undefined behaviour; catch inside the callback.
 *   2. It must not block on anything a thread might hold while calling into
 *      this library. That is the whole lock ordering, and it is the C pendant
 *      of the Python door's "the GIL must be outermost".
 *   3. It must not call ws_station_free.
 *
 * The other entry points may be called from it. */
typedef void (*ws_log_callback)(int level, const char *target, const char *message,
                                void *user_data);

/* Install `callback` as the destination for this library's reporting and
 * aimdb's.
 *
 * Returns true if this call installed it, false if a sink was already in
 * place. Never aborts: a second call is something a library inside a library
 * does all the time.
 *
 * `filter` uses tracing's EnvFilter syntax; NULL means RUST_LOG, falling back
 * to "info". It gates *below* the callback — events it drops never cross.
 *
 * The sink CANNOT BE UNINSTALLED. `callback` and `user_data` must remain valid
 * for the lifetime of the process, which in practice means this library must
 * not be dlclose()d once it has been called. See README.md. */
bool ws_init_logging(const char *filter, ws_log_callback callback, void *user_data);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* WEATHER_STATION_H */
