//! The pyo3 door onto [`StationHandle`].
//!
//! Not the wheel: no maturin metadata, no build matrix, no distribution name.
//! Findings are in `README.md`.
//!
//! **The GIL is the outermost lock.** After [`init_logging`], aimdb's runtime
//! thread calls into Python, so never hold the GIL while acquiring anything
//! that thread can block on. Every wait on it — `close` included, which joins
//! it — goes through [`Python::detach`], and `is_closed` never takes the mutex
//! `shutdown` holds. No signature can carry that, hence the note.
//!
//! The bridge is a `log::Log`, not a `tracing` subscriber (design 050): a
//! subscriber is the host application's to install, not this module's.

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
        // `Closed` stays a plain `StationError`, as it was when this layer
        // raised it from its own pre-check: a closed station is the ordinary
        // end of a run, not a third thing to catch. `StationErrorKind` is
        // `#[non_exhaustive]`, so an unknown kind lands here too.
        _ => StationError::new_err(message),
    }
}

/// A station's seat in the mesh: join, publish, close.
///
/// ```python
/// with Station.open_profile("station.toml") as station:
///     station.publish_temperature(21.5)
/// ```
///
// `frozen` drops the runtime borrow flag, which is what lets `close()` run
// while a publish is in flight — a `&mut self` method cannot win an exclusive
// borrow from a `&self` one parked in `Python::detach`, and would fail with
// "Already borrowed".
#[pyclass(frozen, name = "Station", module = "weather_station")]
struct PyStation {
    inner: StationHandle,
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
        py.detach(|| self.inner.publish_temperature(celsius))
            .map_err(to_py_err)
    }

    /// Publish a humidity reading, waiting for room in the slot's buffer.
    fn publish_humidity(&self, py: Python<'_>, percent: f32) -> PyResult<()> {
        py.detach(|| self.inner.publish_humidity(percent))
            .map_err(to_py_err)
    }

    /// Publish a temperature reading, or fail rather than wait.
    ///
    /// Does not release the GIL, because it does not block.
    fn try_publish_temperature(&self, celsius: f32) -> PyResult<()> {
        self.inner
            .try_publish_temperature(celsius)
            .map_err(to_py_err)
    }

    /// Publish a humidity reading, or fail rather than wait.
    ///
    /// Does not release the GIL, because it does not block.
    fn try_publish_humidity(&self, percent: f32) -> PyResult<()> {
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

    /// The coordinates the mesh published for this station, or `None` when the
    /// profile omits them.
    ///
    /// Read back through the boundary rather than re-parsed: `open_profile`
    /// already parsed `[app]`, and a second parse could disagree with the
    /// station it is publishing through.
    ///
    /// Still answers after `close()` — see [`slot`](Self::slot).
    #[getter]
    fn lat(&self) -> Option<f64> {
        self.inner.mesh_slot().lat()
    }

    /// See [`lat`](Self::lat).
    #[getter]
    fn lon(&self) -> Option<f64> {
        self.inner.mesh_slot().lon()
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

/// Forwards events into Python's `logging`, so levels stay a runtime question
/// the operator answers — rather than a subscriber this module installs.
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

        directives.sort_by_key(|a| core::cmp::Reverse(a.0.len()));

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
/// Returns `True` if this call installed the bridge, `False` if one was
/// already in place. Never raises; calling it twice is ordinary.
///
/// Events arrive on loggers named after the emitting Rust module, `::`
/// translated to `.`, so each subsystem is separately addressable:
///
/// ```python
/// import logging, weather_station
/// logging.basicConfig(level=logging.INFO)
/// weather_station.init_logging()
/// logging.getLogger("aimdb_core").setLevel(logging.WARNING)
/// ```
///
/// `filter` is the gate below Python — events it drops never acquire the GIL.
/// Comma-separated `level` and `target=level` items
/// (`info,aimdb_core.builder=debug`; `::` works too); defaults to `RUST_LOG`,
/// then `info`. Not `EnvFilter` syntax. Python's own levels refine it.
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
