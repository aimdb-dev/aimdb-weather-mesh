//! The pyo3 door onto [`StationHandle`], built as a spike.
//!
//! Not the wheel: no maturin metadata, no build matrix, no distribution name.
//! It exists to find out what an FFI layer needs from the station crates
//! before they reach a registry. Findings are in `README.md`.
//!
//! # The GIL is the outermost lock
//!
//! [`init_logging`] makes aimdb's runtime thread call into Python to log. From
//! that point on, every wait this module performs is part of a lock ordering,
//! and the rule is stronger than "release the GIL before joining a thread":
//!
//! > Never hold the GIL while acquiring anything the runtime thread can block
//! > on. The GIL must be outermost.
//!
//! Concretely: every method that can wait on the runtime thread — including
//! `close`, which joins it — wraps that wait in [`Python::detach`]. And
//! `StationHandle::is_closed` reads an atomic rather than the mutex `shutdown`
//! holds, so a getter called under the GIL can never block behind a shutdown
//! that is itself waiting for the GIL to be released.
//!
//! None of this is visible in `StationHandle`'s signature — Rust has no GIL, so
//! no type can carry the constraint. It is written down here and exercised by
//! `python/spike.py`.

use std::fmt::Write as _;
use std::path::PathBuf;

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::wrap_pyfunction;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

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
/// `frozen` is what pyo3 offers for a class shared across threads, and this one
/// is: the supported shape is one reader thread per sensor publishing through a
/// single seat. It drops the runtime borrow flag a non-frozen pyclass carries,
/// which is what lets `close()` run while a publish is in flight — a `&mut
/// self` method cannot win an exclusive borrow from a `&self` method that is
/// parked in `Python::detach`, and fails with "Already borrowed" instead.
///
/// The `Option<StationHandle>` this class used to hold is gone with it:
/// `StationHandle::shutdown` takes `&self`, is idempotent, and tracks its own
/// closed flag, so the binding no longer re-solves any of that. The C ABI layer
/// gets the same three properties for free.
#[pyclass(frozen, name = "Station", module = "weather_station")]
struct PyStation {
    inner: StationHandle,
}

impl PyStation {
    /// Refuse a call that needs a live runtime, with a message that says so.
    ///
    /// Best-effort, and deliberately not a lock: `is_closed` reads an atomic,
    /// so a close racing this check merely means the call fails one layer down
    /// with aimdb's own "runtime thread has shut down" instead. This is about
    /// the message, not about correctness — the producers refuse a publish
    /// after close on their own, because dropping the handle releases the last
    /// reference to the database their weak ones point at.
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
    /// implementing `os.PathLike` work, which is what a Python caller reaches
    /// for. Nothing changes on the Rust side: `StationHandle::open_profile`
    /// already takes `impl AsRef<Path>`.
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
    /// Still answers after `close()`. Deliberate: the slot and the name come
    /// from the profile, not from the runtime, and a closed station is a thing
    /// a log line or a traceback still wants to identify. Ask
    /// [`closed`](Self::closed) for the state.
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
    /// arrive — see `StationHandle::close`.
    ///
    /// `Python::detach` is required here, not optional: the shutdown joins
    /// aimdb's runtime thread, and after [`init_logging`] that thread needs the
    /// GIL to log. Holding it across the join deadlocks. See the module docs.
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

/// Flattens an event's fields into one line: the `message` field, then the
/// rest as `key=value`.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // `Debug` for the `format_args!` tracing records here prints the
            // formatted text, without the quotes a `&str` would pick up.
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }
}

impl MessageVisitor {
    fn finish(self) -> String {
        let mut out = self.message;
        out.push_str(&self.fields);
        out
    }
}

/// A `tracing` layer that forwards events into Python's `logging`.
///
/// This is the whole reason the module no longer exports `init_tracing`. An
/// extension module is a library inside somebody else's application, and
/// process-wide logging is the application's decision. Forwarding gives aimdb's
/// events *more* control than a hardcoded stderr filter did, not less: levels
/// become a runtime question a Python operator answers with the tools they
/// already know.
struct PyLoggingLayer;

/// `logging` level numbers. `TRACE` has no Python equivalent, so it lands below
/// `DEBUG`, where `logging` treats it as a custom level.
fn py_level(level: &Level) -> u8 {
    match *level {
        Level::TRACE => 5,
        Level::DEBUG => 10,
        Level::INFO => 20,
        Level::WARN => 30,
        Level::ERROR => 40,
    }
}

impl<S: Subscriber> Layer<S> for PyLoggingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let message = visitor.finish();
        let level = py_level(meta.level());

        // `tracing` targets are Rust module paths (`aimdb_sync::handle`), and
        // `logging` splits its hierarchy on `.`. Without this translation
        // `getLogger("aimdb_core")` would not be a parent of anything aimdb
        // emits, and setting a level on it would silently do nothing.
        let logger_name = meta.target().replace("::", ".");

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
}

/// Route the station's reporting — and aimdb's — into Python's `logging`.
///
/// Returns `True` if this call installed the bridge, `False` if a subscriber
/// was already in place. Never raises, and never panics: calling it twice is
/// something Python code does all the time.
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
/// It takes `tracing`'s `EnvFilter` syntax, defaults to `RUST_LOG`, and falls
/// back to `info` — the floor the stderr subscriber used to hardcode. Python's
/// own levels do the fine-grained work above it, so only lower this floor when
/// you actually want `debug` volume crossing the boundary.
#[pyfunction]
#[pyo3(signature = (filter = None))]
fn init_logging(filter: Option<&str>) -> bool {
    let env_filter = match filter {
        Some(directives) => EnvFilter::new(directives),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
    };
    tracing_subscriber::registry()
        .with(env_filter)
        .with(PyLoggingLayer)
        .try_init()
        .is_ok()
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
