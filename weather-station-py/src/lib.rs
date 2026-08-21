//! The pyo3 door onto [`StationHandle`], built as a spike.
//!
//! Not the wheel: no maturin metadata, no build matrix, no distribution name.
//! It exists to find out what an FFI layer needs from the station crates
//! before they reach a registry. Findings are in `README.md`.

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::wrap_pyfunction;

// `#[pymodule] fn weather_station` generates a module of that name, which
// shadows the dependency crate inside this file — hence the `::` prefixes.
use ::weather_station::{StationError, StationErrorKind, StationHandle};

/// `#[pyclass]` requires `Send`, and pyo3 shares instances across threads, so
/// the handle needs `Sync` too. Pinned here because an FFI layer is the only
/// consumer that would notice the bound falling away.
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

/// Map a failure onto the exception a caller can act on.
///
/// Dispatches on `kind()` rather than the variants: `StationError` is
/// `#[non_exhaustive]`, so matching it here would need a wildcard and a variant
/// added later would land in it silently.
fn to_py_err(err: StationError) -> PyErr {
    let message = err.to_string();
    match err.kind() {
        StationErrorKind::Profile => ProfileError::new_err(message),
        StationErrorKind::Broker => BrokerError::new_err(message),
        StationErrorKind::Runtime => StationErrorPy::new_err(message),
    }
}

/// A station's seat in the mesh: join, publish, close.
///
/// ```python
/// with Station.open_profile("station.toml") as station:
///     station.publish_temperature(21.5)
/// ```
#[pyclass(name = "Station", module = "weather_station")]
struct PyStation {
    /// `StationHandle::close` consumes the handle, and a `#[pymethods]` method
    /// never receives `self` by value. The `Option` bridges the two: `close()`
    /// takes the handle out, and later calls find `None`. Every FFI layer
    /// binding this type has to do the same.
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
    /// Blocks on the broker pre-flight and the graph's first pump, so it
    /// releases the GIL for the duration.
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

    /// Stop the station and shut its runtime thread down. Idempotent.
    ///
    /// A reading published in the last milliseconds before this does not
    /// arrive — see `StationHandle::close`.
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
/// Installs a *global* subscriber, and bypasses Python's `logging` entirely —
/// bridging the two is a wheel-level question, which is why this is a separate
/// call rather than part of `open_profile`.
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
