//! The pyo3 door onto [`StationHandle`], built as a spike.
//!
//! Design 008 §5.3 argues that a Python station should bind an API that
//! already exists rather than invent one, and §6 defers the wheel to a later
//! tag. This crate is neither the wheel nor a proposal: it is the experiment
//! that answers whether the blocking door survives contact with a foreign
//! caller *before* `aimdb-weather-station` and `aimdb-sync` reach a registry,
//! where changing them costs a version in two repositories.
//!
//! What it deliberately does not have: maturin metadata, a build matrix, a
//! package name, or any of the two-distribution split of decision 6. Those are
//! release engineering, and they wait.
//!
//! Findings are recorded in `README.md` beside this file.

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::wrap_pyfunction;

// `#[pymodule] fn weather_station` generates a module of that name, which
// shadows the dependency crate inside this file — hence the `::` prefixes.
use ::weather_station::{StationError, StationHandle};

/// `#[pyclass]` requires its payload to be `Send`, and pyo3 shares instances
/// across threads, so the handle has to be `Sync` too. Pinned here because an
/// FFI layer is the only consumer that needs it: a Rust station never notices
/// if the bound falls away, and the failure would surface as a build error in
/// the wheel rather than in the crate that caused it.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<StationHandle>();
};

create_exception!(
    weather_station,
    StationErrorPy,
    PyException,
    "A station failed to join the mesh, or to publish into it."
);
create_exception!(
    weather_station,
    ProfileError,
    StationErrorPy,
    "The station profile is missing, malformed, or issued for another format."
);
create_exception!(
    weather_station,
    BrokerError,
    StationErrorPy,
    "The broker could not be reached, or refused this station's credential."
);

/// Map a station failure onto the exception a Python caller can act on.
///
/// The three classes are the three things a caller does differently: fix the
/// file, fix the deployment, or fix nothing because the process is going down.
/// Note the wildcard: `StationError` is `#[non_exhaustive]`, so a mapping
/// written outside the crate cannot be exhaustive and a variant added later
/// silently lands in the fallback.
fn to_py_err(err: StationError) -> PyErr {
    let message = err.to_string();
    match err {
        StationError::UnsupportedProfileVersion { .. }
        | StationError::MalformedStationId(_)
        | StationError::ProfileUnreadable { .. }
        | StationError::ProfileMalformed { .. } => ProfileError::new_err(message),
        StationError::BrokerUrl(_)
        | StationError::CredentialRejected(_)
        | StationError::BrokerUnreachable { .. }
        | StationError::BrokerTimeout(_) => BrokerError::new_err(message),
        _ => StationErrorPy::new_err(message),
    }
}

/// A station's seat in the mesh: join, publish, close.
///
/// ```python
/// from weather_station import Station
///
/// with Station.open_profile("station.toml") as station:
///     station.publish_temperature(21.5)
/// ```
///
/// The handle owns a runtime thread, so an instance is not free to abandon:
/// `close()` — or the context manager, which calls it — shuts the thread down
/// in the caller's own time rather than in a destructor.
#[pyclass(name = "Station", module = "weather_station")]
struct PyStation {
    /// `StationHandle::close` consumes the handle, and a `#[pymethods]` method
    /// never receives `self` by value — only `&self` / `&mut self`. The
    /// `Option` is what bridges the two: `close()` takes the handle out and
    /// consumes it, and every later call finds `None` and says so. Any FFI
    /// layer binding this type has to do the same thing.
    inner: Option<StationHandle>,
}

impl PyStation {
    fn handle(&self) -> PyResult<&StationHandle> {
        self.inner
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("this station is closed"))
    }
}

#[pymethods]
impl PyStation {
    /// Join the mesh from a `station.toml` path.
    ///
    /// Blocks on the broker pre-flight and on the graph's first pump, so the
    /// GIL is released for the duration — a station that joins a slow broker
    /// must not freeze the interpreter it was called from.
    #[staticmethod]
    fn open_profile(py: Python<'_>, path: &str) -> PyResult<Self> {
        let handle = py
            .detach(|| StationHandle::open_profile(path))
            .map_err(to_py_err)?;
        Ok(Self {
            inner: Some(handle),
        })
    }

    /// Publish a temperature reading, waiting for room in the slot's buffer.
    fn publish_temperature(&self, py: Python<'_>, celsius: f32) -> PyResult<()> {
        let handle = self.handle()?;
        py.detach(|| handle.publish_temperature(celsius))
            .map_err(to_py_err)
    }

    /// Publish a humidity reading, waiting for room in the slot's buffer.
    fn publish_humidity(&self, py: Python<'_>, percent: f32) -> PyResult<()> {
        let handle = self.handle()?;
        py.detach(|| handle.publish_humidity(percent))
            .map_err(to_py_err)
    }

    /// Publish a temperature reading, or fail rather than wait.
    fn try_publish_temperature(&self, celsius: f32) -> PyResult<()> {
        self.handle()?
            .try_publish_temperature(celsius)
            .map_err(to_py_err)
    }

    /// Publish a humidity reading, or fail rather than wait.
    fn try_publish_humidity(&self, percent: f32) -> PyResult<()> {
        self.handle()?
            .try_publish_humidity(percent)
            .map_err(to_py_err)
    }

    /// The slot number this station publishes into.
    #[getter]
    fn slot(&self) -> PyResult<u16> {
        Ok(self.handle()?.mesh_slot().slot())
    }

    /// The station's display name, as the profile issued it.
    #[getter]
    fn name(&self) -> PyResult<String> {
        Ok(self.handle()?.mesh_slot().name().to_owned())
    }

    /// Stop the station and shut its runtime thread down.
    ///
    /// Idempotent, so the context manager can call it after the caller already
    /// did.
    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        match self.inner.take() {
            Some(handle) => py.detach(|| handle.close()).map_err(to_py_err),
            None => Ok(()),
        }
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(
        &mut self,
        py: Python<'_>,
        _args: &Bound<'_, pyo3::types::PyTuple>,
    ) -> PyResult<()> {
        self.close(py)
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            Some(handle) => {
                let slot = handle.mesh_slot();
                format!("<Station slot={} name={:?}>", slot.slot(), slot.name())
            }
            None => "<Station closed>".to_owned(),
        }
    }
}

/// Route the station's own reporting to stderr.
///
/// Off unless asked for, because it installs a *global* subscriber: calling it
/// twice is an error, and the output bypasses Python's `logging` entirely.
/// Bridging the two is a wheel-level question, not a spike one — it is the
/// reason this exists as a separate call rather than inside `open_profile`.
#[pyfunction]
#[pyo3(signature = (target = "weather_station"))]
fn init_tracing(target: &str) {
    ::weather_station::init_tracing(target);
}

#[pymodule]
fn weather_station(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyStation>()?;
    m.add_function(wrap_pyfunction!(init_tracing, m)?)?;
    m.add("StationError", m.py().get_type::<StationErrorPy>())?;
    m.add("ProfileError", m.py().get_type::<ProfileError>())?;
    m.add("BrokerError", m.py().get_type::<BrokerError>())?;
    m.add("PROFILE_VERSION", ::weather_station::PROFILE_VERSION)?;
    Ok(())
}
