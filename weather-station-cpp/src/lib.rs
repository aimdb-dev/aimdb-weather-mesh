//! The C ABI door onto [`StationHandle`].
//!
//! Not the shipped library: no soname, no CMake package config, no generated
//! header. The pendant of `weather-station-py`, for the language where the
//! boundary is a C ABI rather than an interpreter. Findings are in `README.md`.
//!
//! # Three rules this layer exists to keep
//!
//! **Nothing unwinds across the boundary.** A Rust panic that reaches a C++
//! frame is undefined behaviour — there is no pyo3 here to turn it into an
//! exception object. Every `extern "C"` function below wraps its body in
//! [`catch_unwind`](std::panic::catch_unwind) and reports [`WS_ERR_PANIC`], and
//! every callback this layer *invokes* is documented `noexcept` on the C++ side
//! for the same reason in the other direction.
//!
//! **Every argument is hostile.** C has no `Option`, no lifetime and no UTF-8
//! guarantee, so a null pointer, a dangling one and a `const char*` that is not
//! UTF-8 all arrive as ordinary calls. Only the first and third are detectable;
//! both are, here.
//!
//! **The callback thread is aimdb's runtime thread.** Once [`ws_init_logging`]
//! is installed, aimdb's runtime thread calls out through a function pointer
//! into the consuming application — the Python door's lock ordering, rewritten
//! as "whatever lock the callback takes". See `README.md`.
//!
//! # The log sink is a `log::Log`, not a subscriber (design 050)
//!
//! This layer installs no `tracing` subscriber. Deciding where a host's
//! diagnostics go is the host's call, and a `tracing::Layer` has nowhere to put
//! the caller's `user_data` besides a static of this library's own — which is
//! what the C++ header used to keep, and what raced. A `log::Log` impl *is* the
//! context, so [`CSink`] carries the callback and the pointer together and
//! `log::set_boxed_logger` makes the first-wins decision once, in Rust.

// The exported types carry their C names, so the header and this file spell
// each type the same way.
#![allow(non_camel_case_types)]

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use log::{Level, LevelFilter};

use weather_station::{StationError, StationErrorKind, StationHandle, PROFILE_VERSION};

/// `ws_station` is handed to C as a pointer and used from several threads at
/// once, so the type behind it has to be `Send + Sync` — with nothing on the C
/// side to check it. Pinned here instead.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<StationHandle>();
};

// ---------------------------------------------------------------------------
// Status codes
// ---------------------------------------------------------------------------

/// The call succeeded.
pub const WS_OK: c_int = 0;
/// The profile is wrong. Edit it, or have it re-issued.
pub const WS_ERR_PROFILE: c_int = 1;
/// The broker is unreachable or refused the credential.
pub const WS_ERR_BROKER: c_int = 2;
/// The station's own machinery or host.
pub const WS_ERR_RUNTIME: c_int = 3;
/// The station has been closed.
pub const WS_ERR_CLOSED: c_int = 4;
/// A null pointer, or a `const char*` that is not UTF-8.
pub const WS_ERR_INVALID_ARGUMENT: c_int = 5;
/// Rust panicked. The call had no effect the caller can rely on, and the
/// station should be considered unusable.
pub const WS_ERR_PANIC: c_int = 6;

/// The ABI this build speaks. Bumped when a symbol changes shape, so a header
/// and a library from different tags refuse each other at startup instead of
/// disagreeing about a struct at run time.
pub const WS_ABI_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// The last error, per thread
// ---------------------------------------------------------------------------

thread_local! {
    /// Where the message goes when the return value is only a code.
    ///
    /// Thread-local because the alternative is a global two publishing threads
    /// overwrite for each other. Owned by this layer and freed on the next
    /// failing call from the same thread.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(message: impl Into<Vec<u8>>) {
    // A message containing an interior NUL cannot cross as a C string; keep the
    // prefix rather than losing the whole message.
    let bytes: Vec<u8> = message.into();
    let cleaned = match CString::new(bytes.clone()) {
        Ok(s) => s,
        Err(err) => {
            let upto = err.nul_position();
            CString::new(&bytes[..upto]).unwrap_or_else(|_| CString::new("(unprintable)").unwrap())
        }
    };
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(cleaned));
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// Record `err` for [`ws_last_error`] and return the code a caller acts on.
///
/// Dispatches on `kind()` rather than the variants: `StationError` is
/// `#[non_exhaustive]`, so matching it here would need a wildcard and a variant
/// added later would land in it silently.
fn report(err: StationError) -> c_int {
    set_last_error(err.to_string());
    match err.kind() {
        StationErrorKind::Profile => WS_ERR_PROFILE,
        StationErrorKind::Broker => WS_ERR_BROKER,
        StationErrorKind::Closed => WS_ERR_CLOSED,
        // `StationErrorKind` is `#[non_exhaustive]`; a kind added upstream is a
        // runtime failure here until this layer is taught otherwise.
        _ => WS_ERR_RUNTIME,
    }
}

/// Run `body`, turning a panic into [`WS_ERR_PANIC`] rather than undefined
/// behaviour.
///
/// The one place this layer can be sure a panic stops. `AssertUnwindSafe` is
/// deliberate: a panic mid-call may leave the station in a state no C caller
/// can reason about, hence a distinct code rather than an ordinary failure.
fn guard(body: impl FnOnce() -> c_int) -> c_int {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(code) => code,
        Err(payload) => {
            let message = panic_message(&payload);
            set_last_error(format!("panic across the FFI boundary: {message}"));
            WS_ERR_PANIC
        }
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic payload)".to_owned()
    }
}

/// A borrowed `const char*` as a `&str`, or `None` for null / non-UTF-8.
///
/// # Safety
/// `ptr` must be null or a NUL-terminated string valid for the call.
unsafe fn cstr(ptr: *const c_char) -> Option<&'static str> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok()
}

// ---------------------------------------------------------------------------
// The station
// ---------------------------------------------------------------------------

/// A station's seat in the mesh, behind a pointer.
///
/// `name` is owned here rather than borrowed from the slot, which returns a
/// `&str` where C wants a NUL. Copied once at open so [`ws_station_name`] can
/// hand back a pointer valid until [`ws_station_free`] without allocating.
pub struct ws_station {
    inner: StationHandle,
    name: CString,
}

/// `ptr` as a `&ws_station`, or `None`.
///
/// # Safety
/// `ptr` must be null or a pointer returned by [`ws_station_open_profile`] that
/// has not yet been passed to [`ws_station_free`].
unsafe fn station<'a>(ptr: *const ws_station) -> Option<&'a ws_station> {
    ptr.as_ref()
}

/// The ABI version this build speaks.
#[no_mangle]
pub extern "C" fn ws_abi_version() -> u32 {
    WS_ABI_VERSION
}

/// The `profile_version` this build of the mesh understands.
#[no_mangle]
pub extern "C" fn ws_profile_version() -> u64 {
    PROFILE_VERSION
}

/// The last failure on *this* thread, or `NULL` if the last call succeeded.
///
/// Valid until the next failing call on the same thread. Never freed by the
/// caller.
#[no_mangle]
pub extern "C" fn ws_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| match slot.borrow().as_ref() {
        Some(message) => message.as_ptr(),
        None => std::ptr::null(),
    })
}

/// Join the mesh from a `station.toml` path.
///
/// Blocks on the broker pre-flight and the graph's first pump. On success
/// `*out` owns a station the caller must eventually pass to
/// [`ws_station_free`].
///
/// `path` must be UTF-8. A shipped library would need a `_w` entry point or a
/// documented encoding rule for Windows; recorded in `README.md`.
///
/// # Safety
/// `path` must be a NUL-terminated string and `out` a writable pointer.
#[no_mangle]
pub unsafe extern "C" fn ws_station_open_profile(
    path: *const c_char,
    out: *mut *mut ws_station,
) -> c_int {
    guard(|| {
        if out.is_null() {
            set_last_error("ws_station_open_profile: out must not be NULL");
            return WS_ERR_INVALID_ARGUMENT;
        }
        // Written before any fallible step, so a caller that ignores the status
        // and reads `*out` finds NULL rather than uninitialised stack.
        *out = std::ptr::null_mut();

        let Some(path) = cstr(path) else {
            set_last_error("ws_station_open_profile: path must be a NUL-terminated UTF-8 string");
            return WS_ERR_INVALID_ARGUMENT;
        };

        match StationHandle::open_profile(PathBuf::from(path)) {
            Ok(handle) => {
                let name = CString::new(handle.mesh_slot().name())
                    .unwrap_or_else(|_| CString::new("(invalid name)").unwrap());
                *out = Box::into_raw(Box::new(ws_station {
                    inner: handle,
                    name,
                }));
                clear_last_error();
                WS_OK
            }
            Err(err) => report(err),
        }
    })
}

macro_rules! publish_fn {
    ($name:ident, $method:ident, $unit:ident) => {
        /// Publish a reading. See the header for blocking behaviour.
        ///
        /// # Safety
        /// `handle` must be a live station pointer.
        #[no_mangle]
        pub unsafe extern "C" fn $name(handle: *const ws_station, $unit: f32) -> c_int {
            guard(|| {
                let Some(station) = station(handle) else {
                    set_last_error(concat!(stringify!($name), ": handle must not be NULL"));
                    return WS_ERR_INVALID_ARGUMENT;
                };
                match station.inner.$method($unit) {
                    Ok(()) => {
                        clear_last_error();
                        WS_OK
                    }
                    Err(err) => report(err),
                }
            })
        }
    };
}

publish_fn!(ws_station_publish_temperature, publish_temperature, celsius);
publish_fn!(ws_station_publish_humidity, publish_humidity, percent);
publish_fn!(
    ws_station_try_publish_temperature,
    try_publish_temperature,
    celsius
);
publish_fn!(
    ws_station_try_publish_humidity,
    try_publish_humidity,
    percent
);

/// The slot number this station publishes into, or `0` for a null handle.
///
/// Still answers after a close: the slot comes from the profile, not the
/// runtime, and a closed station is still worth naming in a log line.
///
/// # Safety
/// `handle` must be null or a live station pointer.
#[no_mangle]
pub unsafe extern "C" fn ws_station_slot(handle: *const ws_station) -> u16 {
    // No status to return, so a panic here cannot be reported — which is why
    // the body is trivial enough not to have one.
    match station(handle) {
        Some(station) => station.inner.mesh_slot().slot(),
        None => 0,
    }
}

/// The station's display name, as the profile issued it. Borrowed: valid until
/// [`ws_station_free`], and never freed by the caller.
///
/// # Safety
/// `handle` must be null or a live station pointer.
#[no_mangle]
pub unsafe extern "C" fn ws_station_name(handle: *const ws_station) -> *const c_char {
    match station(handle) {
        Some(station) => station.name.as_ptr(),
        None => std::ptr::null(),
    }
}

/// Whether the station has been closed. `true` for a null handle, because a
/// station that does not exist is not open.
///
/// Never takes a lock: this is what a caller asks while holding its own, and
/// the answer must not queue behind a shutdown.
///
/// # Safety
/// `handle` must be null or a live station pointer.
#[no_mangle]
pub unsafe extern "C" fn ws_station_is_closed(handle: *const ws_station) -> bool {
    match station(handle) {
        Some(station) => station.inner.is_closed(),
        None => true,
    }
}

/// Stop the station and shut its runtime thread down. Idempotent, and safe to
/// call while other threads publish through the same handle.
///
/// Takes `const ws_station*` on purpose: `StationHandle::shutdown` takes
/// `&self`, so this needs no exclusive access to a handle a publish is using.
///
/// A reading published in the last milliseconds before this does not
/// necessarily arrive; see `README.md`.
///
/// # Safety
/// `handle` must be a live station pointer.
#[no_mangle]
pub unsafe extern "C" fn ws_station_close(handle: *const ws_station) -> c_int {
    guard(|| {
        let Some(station) = station(handle) else {
            set_last_error("ws_station_close: handle must not be NULL");
            return WS_ERR_INVALID_ARGUMENT;
        };
        match station.inner.shutdown() {
            Ok(()) => {
                clear_last_error();
                WS_OK
            }
            Err(err) => report(err),
        }
    })
}

/// Release the station.
///
/// Closes first if the caller did not. Passing `NULL` is a no-op, so this can
/// sit unguarded in a destructor.
///
/// **Not thread-safe against anything else on the same pointer.** Every other
/// entry point takes a shared reference and may be called from any thread; this
/// one consumes the allocation, and C has no borrow checker to say so.
///
/// # Safety
/// `handle` must be null or a pointer from [`ws_station_open_profile`] that has
/// not yet been freed, and no other thread may touch it during or after this
/// call.
#[no_mangle]
pub unsafe extern "C" fn ws_station_free(handle: *mut ws_station) {
    if handle.is_null() {
        return;
    }
    // A panic must not escape a destructor: C++ calls this from `~Station`,
    // and a `~Station` running during unwinding that lets a second exception
    // out calls `std::terminate`.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let station = Box::from_raw(handle);
        // Explicit rather than relying on `Drop`: the drop path logs a warning
        // about an unclosed station, and a destructor is exactly where that
        // warning would be noise.
        let _ = station.inner.shutdown();
        drop(station);
    }));
}

// ---------------------------------------------------------------------------
// The log sink
// ---------------------------------------------------------------------------

/// Level numbers, chosen to match Python's `logging` so the two doors report
/// the same event as the same number. `TRACE` has no Python equivalent and
/// lands below `DEBUG`.
pub const WS_LOG_TRACE: c_int = 5;
pub const WS_LOG_DEBUG: c_int = 10;
pub const WS_LOG_INFO: c_int = 20;
pub const WS_LOG_WARN: c_int = 30;
pub const WS_LOG_ERROR: c_int = 40;

/// What a log sink is called with. `target` is the emitting Rust module path
/// with `::` intact — unlike the Python door, which translates to `.` because
/// `logging` splits its hierarchy there; C has no hierarchy to fit.
///
/// Both strings are borrowed for the duration of the call only.
pub type ws_log_callback = Option<
    unsafe extern "C" fn(
        level: c_int,
        target: *const c_char,
        message: *const c_char,
        user_data: *mut c_void,
    ),
>;

fn c_level(level: Level) -> c_int {
    match level {
        Level::Trace => WS_LOG_TRACE,
        Level::Debug => WS_LOG_DEBUG,
        Level::Info => WS_LOG_INFO,
        Level::Warn => WS_LOG_WARN,
        Level::Error => WS_LOG_ERROR,
    }
}

fn parse_level(name: &str) -> Option<LevelFilter> {
    match name.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::Off),
        "error" => Some(LevelFilter::Error),
        "warn" | "warning" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        "debug" => Some(LevelFilter::Debug),
        "trace" => Some(LevelFilter::Trace),
        _ => None,
    }
}

/// The sink: the callback, the caller's pointer, and the filter, in one value.
///
/// This is the whole point of design 050. Under `tracing` the callback lived in
/// a `Layer` with nowhere to keep `user_data`, so the C++ header kept a static
/// beside it — written while aimdb's runtime thread read it. Here the context
/// *is* the destination, so there is no second place for it to live.
struct CSink {
    callback: unsafe extern "C" fn(c_int, *const c_char, *const c_char, *mut c_void),
    /// Held as a `usize` so the struct is plainly `Send + Sync`: the pointer is
    /// opaque to this layer, the C side owns whatever it addresses, and the
    /// contract requires it to outlive the process.
    user_data: usize,
    /// `(target prefix, level)`, longest prefix first, so the first match wins.
    directives: Vec<(String, LevelFilter)>,
    default_level: LevelFilter,
}

impl CSink {
    /// The filter grammar `EnvFilter` left behind: comma-separated items, each
    /// either a bare level (the default for everything) or `target=level`.
    /// `aimdb_core=info,aimdb_core::builder=debug` does what it looks like.
    ///
    /// Deliberately not `EnvFilter`: matching a prefix list is thirty lines and
    /// keeps `tracing-subscriber` out of a library that is loaded into somebody
    /// else's process. Anything unparseable is skipped rather than rejected —
    /// a filter typo should not silence a station.
    fn new(
        callback: unsafe extern "C" fn(c_int, *const c_char, *const c_char, *mut c_void),
        user_data: *mut c_void,
        filter: Option<&str>,
    ) -> Self {
        let mut directives: Vec<(String, LevelFilter)> = Vec::new();
        let mut default_level = LevelFilter::Info;

        for item in filter.unwrap_or("info").split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            match item.split_once('=') {
                Some((target, level)) => {
                    if let Some(level) = parse_level(level) {
                        directives.push((target.trim().to_string(), level));
                    }
                }
                None => {
                    if let Some(level) = parse_level(item) {
                        default_level = level;
                    }
                }
            }
        }

        // Longest first: `aimdb_core::builder=debug` must beat `aimdb_core=warn`.
        directives.sort_by_key(|a| core::cmp::Reverse(a.0.len()));

        Self {
            callback,
            user_data: user_data as usize,
            directives,
            default_level,
        }
    }

    fn level_for(&self, target: &str) -> LevelFilter {
        self.directives
            .iter()
            .find(|(prefix, _)| target.starts_with(prefix.as_str()))
            .map(|(_, level)| *level)
            .unwrap_or(self.default_level)
    }

    /// The loosest level any directive admits — what `set_max_level` is set to,
    /// so the cheap global gate never drops an event a directive wanted.
    fn max_level(&self) -> LevelFilter {
        self.directives
            .iter()
            .map(|(_, level)| *level)
            .chain(std::iter::once(self.default_level))
            .max()
            .unwrap_or(LevelFilter::Info)
    }
}

impl log::Log for CSink {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.level_for(metadata.target())
    }

    fn log(&self, record: &log::Record<'_>) {
        // `log` checks `set_max_level` but not `enabled`, so the per-target
        // directives are applied here.
        if !self.enabled(record.metadata()) {
            return;
        }

        // Interior NULs would truncate the message; an event is not worth a
        // failure, so they are replaced rather than dropped.
        let message = CString::new(record.args().to_string().replace('\0', "?"))
            .unwrap_or_else(|_| CString::new("(unprintable event)").unwrap());
        let target = CString::new(record.target().replace('\0', "?"))
            .unwrap_or_else(|_| CString::new("(unprintable target)").unwrap());

        // Runs on whatever thread emitted the event — aimdb's runtime thread
        // included. Whatever the callback locks, it locks *from that thread*,
        // which is the lock ordering `README.md` is about.
        //
        // The callback is required to be `noexcept`: a C++ exception unwinding
        // out of here crosses Rust frames, which is undefined behaviour. The
        // C++ header's trampoline catches; this catches a *Rust* panic, which
        // is the only half this side can see.
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
            (self.callback)(
                c_level(record.level()),
                target.as_ptr(),
                message.as_ptr(),
                self.user_data as *mut c_void,
            );
        }));
    }

    fn flush(&self) {}
}

/// Route the station's reporting — and aimdb's — to `callback`.
///
/// Returns `true` if this call installed the sink, `false` if one was already
/// in place. Never panics and never aborts: a second call is ordinary, and
/// there is no exception type here to carry a complaint. The decision is
/// `log::set_boxed_logger`'s, so it is the same decision for every binding and
/// no layer above can disagree with it.
///
/// `filter` is a comma-separated list of `level` and `target=level` items
/// (`info,aimdb_core::builder=debug`), defaults to `RUST_LOG` when `NULL`, and
/// falls back to `info`. It is the cheap gate *below* the callback: events it
/// drops never reach C at all. It is **not** `tracing`'s `EnvFilter` syntax —
/// spans, field filters and regex directives are gone; see `README.md`.
///
/// `user_data` is passed back untouched and **must outlive the process**. The
/// sink cannot be uninstalled — see the module docs.
///
/// # Safety
/// `callback` must be safe to call from any thread, must not unwind, and
/// `user_data` must remain valid for as long as the process runs.
#[no_mangle]
pub unsafe extern "C" fn ws_init_logging(
    filter: *const c_char,
    callback: ws_log_callback,
    user_data: *mut c_void,
) -> bool {
    let installed = guard(|| {
        let Some(callback) = callback else {
            return 1;
        };
        let from_env;
        let filter = match cstr(filter) {
            Some(directives) => Some(directives),
            None => {
                from_env = std::env::var("RUST_LOG").ok();
                from_env.as_deref()
            }
        };

        let sink = CSink::new(callback, user_data, filter);
        let max = sink.max_level();

        // `set_boxed_logger` rather than `Box::leak` + `set_logger`: it leaks on
        // success and drops on refusal, so the losing caller's sink does not
        // linger.
        if log::set_boxed_logger(Box::new(sink)).is_err() {
            return 1;
        }
        log::set_max_level(max);
        0
    });
    installed == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // Addresses of statics, used as opaque cookies. They outlive the process,
    // which is exactly what the C contract demands of `user_data`.
    static FIRST_COOKIE: u8 = 1;
    static SECOND_COOKIE: u8 = 2;

    static FIRST_CALLS: AtomicUsize = AtomicUsize::new(0);
    static REFUSED_CALLS: AtomicUsize = AtomicUsize::new(0);
    static WRONG_USER_DATA: AtomicUsize = AtomicUsize::new(0);
    static TARGETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn cookie(of: &'static u8) -> *mut c_void {
        of as *const u8 as *mut c_void
    }

    unsafe extern "C" fn first_sink(
        _level: c_int,
        target: *const c_char,
        _message: *const c_char,
        user_data: *mut c_void,
    ) {
        if user_data != cookie(&FIRST_COOKIE) {
            WRONG_USER_DATA.fetch_add(1, Ordering::Relaxed);
        }
        FIRST_CALLS.fetch_add(1, Ordering::Relaxed);
        TARGETS.lock().unwrap().push(
            unsafe { CStr::from_ptr(target) }
                .to_string_lossy()
                .into_owned(),
        );
    }

    /// Installed by a second `ws_init_logging`, which must be refused. Every
    /// call here is a first caller whose sink was replaced behind its back.
    unsafe extern "C" fn refused_sink(
        _level: c_int,
        _target: *const c_char,
        _message: *const c_char,
        _user_data: *mut c_void,
    ) {
        REFUSED_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    /// Design 050: the two defects the C++ header used to have, asserted where
    /// they can be asserted without a broker or a C++ toolchain.
    ///
    /// The header kept a `SinkHolder` static beside the callback, because a
    /// `tracing::Layer` had nowhere to put `user_data`. It was assigned on
    /// every call — before asking whether the install would be accepted, so a
    /// second caller replaced the first caller's sink and then returned `false`
    /// to say it had not — and assigned from the calling thread while the
    /// trampoline read it from aimdb's runtime thread.
    ///
    /// One test, not three: `log::set_boxed_logger` is once per process, and
    /// every test in this binary shares that process.
    #[test]
    fn a_refused_install_leaves_the_first_sink_receiving() {
        let filter = CString::new("trace").expect("filter");

        // SAFETY: both callbacks are `extern "C"`, neither unwinds, and each
        // cookie is the address of a `static` that outlives the process.
        unsafe {
            assert!(
                ws_init_logging(filter.as_ptr(), Some(first_sink), cookie(&FIRST_COOKIE)),
                "the first install must be accepted"
            );
            log::info!("before the second install");

            assert!(
                !ws_init_logging(filter.as_ptr(), Some(refused_sink), cookie(&SECOND_COOKIE)),
                "a second install must be refused"
            );
            log::info!("after the second install");
        }

        assert_eq!(
            REFUSED_CALLS.load(Ordering::Relaxed),
            0,
            "a refused install replaced the first caller's sink"
        );
        assert!(
            FIRST_CALLS.load(Ordering::Relaxed) >= 2,
            "the first sink stopped receiving after a refused second install"
        );
        // Not vacuous: the count above proves the sink was called at all, so a
        // zero here means the pointer arrived, not that nobody looked.
        assert_eq!(
            WRONG_USER_DATA.load(Ordering::Relaxed),
            0,
            "user_data did not travel with the callback"
        );
        assert!(
            TARGETS
                .lock()
                .unwrap()
                .iter()
                .any(|t| t.starts_with("weather_station_ffi")),
            "the emitting module did not reach C as the target"
        );
    }
}
