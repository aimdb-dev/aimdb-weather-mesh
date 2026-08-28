//! The pyo3 door onto [`StationHandle`], built as a spike.
//!
//! Not the wheel: no maturin metadata, no build matrix, no distribution name.
//! Findings are in `README.md`.
//!
//! # The GIL is the outermost lock
//!
//! [`init_logging`] makes aimdb's runtime thread call into Python to log, so
//! from that point on every wait this module performs is part of a lock
//! ordering:
//!
//! > Never hold the GIL while acquiring anything the runtime thread can block
//! > on. The GIL must be outermost.
//!
//! Concretely: every method that can wait on the runtime thread — `close`
//! included, which joins it — wraps that wait in [`Python::detach`]. And
//! `StationHandle::is_closed` never takes the mutex `shutdown` holds, so a
//! getter called under the GIL cannot block behind a shutdown that is itself
//! waiting for the GIL.
//!
//! Rust has no GIL, so no signature can carry the constraint. It is written
//! down here and exercised by `python/spike.py`.
//!
//! # The bridge is a `log::Log`, not a subscriber
//!
//! [`init_logging`] used to install the process's global `tracing` subscriber —
//! the trespass dropping `init_tracing` was meant to end, committed one layer
//! down. Design 050 gave the facade a second destination, so it installs an
//! ordinary `log::Log` this module owns and `tracing-subscriber` is out of the
//! extension's dependency graph.

use std::path::PathBuf;

use log::{Level, LevelFilter};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::wrap_pyfunction;

// `#[pymodule] fn weather_station` generates a module of that name, which
// shadows the dependency crate inside this file — hence the `::` prefixes.
//
// `StationError` is imported under another name so `create_exception!` can have
// the good one: that macro takes the *Python* class name from the Rust
// identifier, so a Rust-side clash would otherwise show up in every traceback.
use ::weather_station::{
    StationError as CoreStationError, StationErrorKind, StationHandle, PROFILE_VERSION,
};

/// `#[pyclass]` requires `Send`, and pyo3 shares instances across threads, so
/// the handle needs `Sync` too. Pinned here because an FFI layer is the only
/// consumer that would notice the bound falling away.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<StationHandle>();
};

create_exception!(
    weather_station,
    StationError,
    PyException,
    "A station failed to join the mesh, or to publish into it."
);
create_exception!(
    weather_station,
    ProfileError,
    StationError,
    "The station profile is missing, malformed, or issued for another format."
);
create_exception!(
    weather_station,
    BrokerError,
    StationError,
    "The broker could not be reached, or refused this station's credential."
);

/// Map a failure onto the exception a caller can act on.
///
/// Dispatches on `kind()` rather than the variants: `StationError` is
/// `#[non_exhaustive]`, so matching it here would need a wildcard and a variant
/// added later would land in it silently.
fn to_py_err(err: CoreStationError) -> PyErr {
    let message = err.to_string();
    match err.kind() {
        StationErrorKind::Profile => ProfileError::new_err(message),
        StationErrorKind::Broker => BrokerError::new_err(message),
        StationErrorKind::Runtime => StationError::new_err(message),
    }
}

/// A station's seat in the mesh: join, publish, close.
///
/// ```python
/// with Station.open_profile("station.toml") as station:
///     station.publish_temperature(21.5)
/// ```
///
/// `frozen`, because the supported shape is one reader thread per sensor
/// publishing through a single seat. It drops the runtime borrow flag a
/// non-frozen pyclass carries, which is what lets `close()` run while a publish
/// is in flight — otherwise a `&mut self` method cannot win an exclusive borrow
/// from a `&self` method parked in `Python::detach`, and fails with "Already
/// borrowed".
#[pyclass(frozen, name = "Station", module = "weather_station")]
struct PyStation {
    inner: StationHandle,
}

impl PyStation {
    /// Refuse a call that needs a live runtime, with a message that says so.
    ///
    /// Best-effort, and deliberately not a lock: a close racing this check
    /// merely means the call fails one layer down with aimdb's own "runtime
    /// thread has shut down". This is about the message, not correctness — the
    /// producers refuse a publish after close on their own.
    fn ensure_open(&self) -> PyResult<()> {
        if self.inner.is_closed() {
            return Err(StationError::new_err("this station is closed"));
        }
        Ok(())
    }
}

#[pymethods]
impl PyStation {
    /// Join the mesh from a `station.toml` path.
    ///
    /// Blocks on the broker pre-flight and the graph's first pump, so it
    /// releases the GIL for the duration.
    ///
    /// Takes `PathBuf` rather than `&str` so `pathlib.Path` and anything else
    /// implementing `os.PathLike` work.
    #[staticmethod]
    fn open_profile(py: Python<'_>, path: PathBuf) -> PyResult<Self> {
        let handle = py
            .detach(|| StationHandle::open_profile(&path))
            .map_err(to_py_err)?;
        Ok(Self { inner: handle })
    }

    /// Publish a temperature reading, waiting for room in the slot's buffer.
    fn publish_temperature(&self, py: Python<'_>, celsius: f32) -> PyResult<()> {
        self.ensure_open()?;
        py.detach(|| self.inner.publish_temperature(celsius))
            .map_err(to_py_err)
    }

    /// Publish a humidity reading, waiting for room in the slot's buffer.
    fn publish_humidity(&self, py: Python<'_>, percent: f32) -> PyResult<()> {
        self.ensure_open()?;
        py.detach(|| self.inner.publish_humidity(percent))
            .map_err(to_py_err)
    }

    /// Publish a temperature reading, or fail rather than wait.
    ///
    /// Does not release the GIL, because it does not block.
    fn try_publish_temperature(&self, celsius: f32) -> PyResult<()> {
        self.ensure_open()?;
        self.inner
            .try_publish_temperature(celsius)
            .map_err(to_py_err)
    }

    /// Publish a humidity reading, or fail rather than wait.
    ///
    /// Does not release the GIL, because it does not block.
    fn try_publish_humidity(&self, percent: f32) -> PyResult<()> {
        self.ensure_open()?;
        self.inner.try_publish_humidity(percent).map_err(to_py_err)
    }

    /// The slot number this station publishes into.
    ///
    /// Still answers after `close()`: the slot and the name come from the
    /// profile, not the runtime, and a closed station is still worth naming in
    /// a traceback. Ask [`closed`](Self::closed) for the state.
    #[getter]
    fn slot(&self) -> u16 {
        self.inner.mesh_slot().slot()
    }

    /// The station's display name, as the profile issued it.
    ///
    /// Still answers after `close()` — see [`slot`](Self::slot).
    #[getter]
    fn name(&self) -> String {
        self.inner.mesh_slot().name().to_owned()
    }

    /// Whether `close()` has run.
    #[getter]
    fn closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Stop the station and shut its runtime thread down. Idempotent.
    ///
    /// A reading published in the last milliseconds before this does not
    /// necessarily arrive — see `StationHandle::close`.
    ///
    /// `Python::detach` is required here, not optional: the shutdown joins
    /// aimdb's runtime thread, and after [`init_logging`] that thread needs the
    /// GIL to log. Holding it across the join deadlocks.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.shutdown()).map_err(to_py_err)
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python<'_>, _args: &Bound<'_, pyo3::types::PyTuple>) -> PyResult<()> {
        self.close(py)
    }

    fn __repr__(&self) -> String {
        let slot = self.inner.mesh_slot();
        let state = if self.inner.is_closed() {
            " closed"
        } else {
            ""
        };
        format!(
            "<Station slot={} name={:?}{}>",
            slot.slot(),
            slot.name(),
            state
        )
    }
}

/// The bridge: a `log` destination that forwards events into Python's
/// `logging`.
///
/// Why the module exports no `init_tracing`: an extension module is a library
/// inside somebody else's application, and process-wide logging is that
/// application's decision. Forwarding makes levels a runtime question a Python
/// operator answers with the tools they already know. Reaching a
/// `tracing::Layer` meant installing that application's subscriber to get here,
/// which is why this is a `log::Log` instead.
struct PyLogger {
    /// `(target prefix, level)`, longest prefix first, so the first match wins.
    directives: Vec<(String, LevelFilter)>,
    default_level: LevelFilter,
}

/// `logging` level numbers. `TRACE` has no Python equivalent, so it lands below
/// `DEBUG`, where `logging` treats it as a custom level.
fn py_level(level: Level) -> u8 {
    match level {
        Level::Trace => 5,
        Level::Debug => 10,
        Level::Info => 20,
        Level::Warn => 30,
        Level::Error => 40,
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

impl PyLogger {
    /// The filter grammar `EnvFilter` left behind: comma-separated items, each
    /// either a bare level or `target=level`. Anything unparseable is skipped
    /// rather than rejected. Coarser than `EnvFilter`, and it matters less here
    /// than at the C door — this is only the floor below Python's own levels.
    fn new(filter: Option<&str>) -> Self {
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
                        // Either spelling. A Python caller thinks in the dotted
                        // logger names this bridge hands out, but the target
                        // matched at log time is the Rust path — so translate
                        // once here rather than per event.
                        directives.push((target.trim().replace('.', "::"), level));
                    }
                }
                None => {
                    if let Some(level) = parse_level(item) {
                        default_level = level;
                    }
                }
            }
        }

        directives.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

        Self {
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

    /// The loosest level any directive admits — what `set_max_level` becomes, so
    /// the cheap global gate never drops an event a directive wanted.
    fn max_level(&self) -> LevelFilter {
        self.directives
            .iter()
            .map(|(_, level)| *level)
            .chain(std::iter::once(self.default_level))
            .max()
            .unwrap_or(LevelFilter::Info)
    }
}

impl log::Log for PyLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.level_for(metadata.target())
    }

    fn log(&self, record: &log::Record<'_>) {
        // `log` checks `set_max_level` but not `enabled`, so the per-target
        // directives are applied here — before the GIL is acquired, which is the
        // point of having a gate below Python at all.
        if !self.enabled(record.metadata()) {
            return;
        }

        let level = py_level(record.level());
        let message = record.args().to_string();

        // Targets are Rust module paths (`aimdb_sync::handle`), and `logging`
        // splits its hierarchy on `.`. Without this translation
        // `getLogger("aimdb_core")` would not be a parent of anything aimdb
        // emits, and setting a level on it would silently do nothing.
        let logger_name = record.target().replace("::", ".");

        // Runs on whatever thread emitted the event — aimdb's runtime thread
        // included, which holds no GIL of its own. That is the acquisition the
        // module docs' lock ordering is about.
        Python::attach(|py| {
            let forward = || -> PyResult<()> {
                let logging = py.import("logging")?;
                let logger = logging.call_method1("getLogger", (logger_name.as_str(),))?;
                logger.call_method1("log", (level, message.as_str()))?;
                Ok(())
            };
            // A logging handler that raises must not unwind into the emitting
            // Rust thread; `logging` already reports handler failures itself.
            if let Err(err) = forward() {
                err.restore(py);
                unsafe { pyo3::ffi::PyErr_WriteUnraisable(std::ptr::null_mut()) };
            }
        });
    }

    fn flush(&self) {}
}

/// Route the station's reporting — and aimdb's — into Python's `logging`.
///
/// Returns `True` if this call installed the bridge, `False` if a destination
/// was already in place. Never raises, and never panics: calling it twice is
/// something Python code does all the time. `log::set_boxed_logger` is what
/// makes that answer honest, and it is the same answer for the C door.
///
/// Events arrive on loggers named after the emitting Rust module, with `::`
/// translated to `.`, so `weather_station`, `aimdb_core.router` and
/// `aimdb_sync.handle` are all separately addressable:
///
/// ```python
/// import logging, weather_station
/// logging.basicConfig(level=logging.INFO)
/// weather_station.init_logging()
/// logging.getLogger("aimdb_core").setLevel(logging.WARNING)
/// ```
///
/// `filter` is the cheap gate *below* Python: events it drops never acquire the
/// GIL at all, which matters because the bridge runs on aimdb's runtime thread.
/// It takes comma-separated `level` and `target=level` items
/// (`info,aimdb_core.builder=debug`; `::` works too), defaults to `RUST_LOG`,
/// and falls back to `info`. Not `tracing`'s `EnvFilter` syntax — see
/// `README.md`. Python's own levels do the fine-grained work above it.
#[pyfunction]
#[pyo3(signature = (filter = None))]
fn init_logging(filter: Option<&str>) -> bool {
    let from_env;
    let filter = match filter {
        Some(directives) => Some(directives),
        None => {
            from_env = std::env::var("RUST_LOG").ok();
            from_env.as_deref()
        }
    };

    let logger = PyLogger::new(filter);
    let max = logger.max_level();

    // `set_boxed_logger` rather than `Box::leak` + `set_logger`: it leaks on
    // success and drops on refusal, so a losing second call keeps nothing alive.
    if log::set_boxed_logger(Box::new(logger)).is_err() {
        return false;
    }
    log::set_max_level(max);
    true
}

#[pymodule]
fn weather_station(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyStation>()?;
    m.add_function(wrap_pyfunction!(init_logging, m)?)?;
    m.add("StationError", m.py().get_type::<StationError>())?;
    m.add("ProfileError", m.py().get_type::<ProfileError>())?;
    m.add("BrokerError", m.py().get_type::<BrokerError>())?;
    m.add("PROFILE_VERSION", PROFILE_VERSION)?;
    Ok(())
}
