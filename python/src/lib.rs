use babushka::start_socket_listener;
use pyo3::exceptions::PyUnicodeDecodeError;
use pyo3::prelude::*;
use pyo3::types::{PyList, PyString};
use pyo3::{PyErr, Python};
use redis::aio::MultiplexedConnection;
use redis::Value;
use redis::{AsyncCommands, RedisResult};

#[pyclass]
struct AsyncClient {
    multiplexer: MultiplexedConnection,
}

#[pyclass]
#[derive(PartialEq, Eq, PartialOrd, Clone)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

#[allow(dead_code)]
#[pymethods]
impl Level {
    fn is_lower(&self, level: &Level) -> bool {
        self <= level
    }
}

fn to_py_err(err: impl std::error::Error) -> PyErr {
    PyErr::new::<PyString, _>(err.to_string())
}

#[pymethods]
impl AsyncClient {
    #[staticmethod]
    fn create_client(address: String, py: Python) -> PyResult<&PyAny> {
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let client = redis::Client::open(address).map_err(to_py_err)?;
            let multiplexer = client
                .get_multiplexed_async_connection()
                .await
                .map_err(to_py_err)?;
            let client = AsyncClient { multiplexer };
            Ok(Python::with_gil(|py| client.into_py(py)))
        })
    }

    fn get<'a>(&self, key: String, py: Python<'a>) -> PyResult<&'a PyAny> {
        let mut connection = self.multiplexer.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let result: RedisResult<Option<String>> = connection.get(key).await;
            result
                .map_err(to_py_err)
                .map(|result| Python::with_gil(|py| result.into_py(py)))
        })
    }

    fn set<'a>(&self, key: String, value: String, py: Python<'a>) -> PyResult<&'a PyAny> {
        let mut connection = self.multiplexer.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let result: RedisResult<()> = connection.set(key, value).await;
            result
                .map_err(to_py_err)
                .map(|_| Python::with_gil(|py| py.None()))
        })
    }

    fn create_pipeline(&self) -> AsyncPipeline {
        AsyncPipeline::new(self.multiplexer.clone())
    }
}

#[pyclass]
struct AsyncPipeline {
    internal_pipeline: redis::Pipeline,
    multiplexer: MultiplexedConnection,
}

impl AsyncPipeline {
    fn new(multiplexer: MultiplexedConnection) -> Self {
        AsyncPipeline {
            internal_pipeline: redis::Pipeline::new(),
            multiplexer,
        }
    }
}

#[pymethods]
impl AsyncPipeline {
    fn get(this: &PyCell<Self>, key: String) -> &PyCell<Self> {
        let mut pipeline = this.borrow_mut();
        pipeline.internal_pipeline.get(key);
        this
    }

    #[pyo3(signature = (key, value, ignore_result = false))]
    fn set(this: &PyCell<Self>, key: String, value: String, ignore_result: bool) -> &PyCell<Self> {
        let mut pipeline = this.borrow_mut();
        pipeline.internal_pipeline.set(key, value);
        if ignore_result {
            pipeline.internal_pipeline.ignore();
        }
        this
    }

    fn execute<'a>(&self, py: Python<'a>) -> PyResult<&'a PyAny> {
        let mut connection = self.multiplexer.clone();
        let pipeline = self.internal_pipeline.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let result: RedisResult<Vec<String>> = pipeline.query_async(&mut connection).await;
            result
                .map_err(to_py_err)
                .map(|results| Python::with_gil(|py| results.into_py(py)))
        })
    }
}

/// A Python module implemented in Rust.
#[pymodule]
fn pybushka(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<AsyncClient>()?;
    m.add_class::<Level>()?;

    #[pyfn(m)]
    fn py_log(log_level: Level, log_identifier: String, message: String) {
        log(log_level, log_identifier, message);
    }

    #[pyfn(m)]
    fn py_init(level: Option<Level>, file_name: Option<&str>) -> Level {
        init(level, file_name)
    }

    #[pyfn(m)]
    fn start_socket_listener_external(init_callback: PyObject) -> PyResult<PyObject> {
        start_socket_listener(move |socket_path| {
            Python::with_gil(|py| {
                match socket_path {
                    Ok(path) => {
                        let _ = init_callback.call(py, (path, py.None()), None);
                    }
                    Err(error_message) => {
                        let _ = init_callback.call(py, (py.None(), error_message), None);
                    }
                };
            });
        });
        Ok(Python::with_gil(|py| "OK".into_py(py)))
    }

    fn redis_value_to_py(py: Python, val: Value) -> PyResult<PyObject> {
        match val {
            Value::Nil => Ok(py.None()),
            Value::Status(str) => Ok(str.into_py(py)),
            Value::Okay => Ok("OK".into_py(py)),
            Value::Int(num) => Ok(num.into_py(py)),
            Value::Data(data) => match std::str::from_utf8(data.as_ref()) {
                Ok(val) => Ok(val.into_py(py)),
                Err(_err) => Err(PyUnicodeDecodeError::new_err(data)),
            },
            Value::Bulk(bulk) => {
                let elements: &PyList = PyList::new(
                    py,
                    bulk.into_iter()
                        .map(|item| redis_value_to_py(py, item).unwrap()),
                );
                Ok(elements.into_py(py))
            }
        }
    }

    #[pyfn(m)]
    pub fn value_from_pointer(py: Python, pointer: u64) -> PyResult<PyObject> {
        let value = unsafe { Box::from_raw(pointer as *mut Value) };
        redis_value_to_py(py, *value)
    }

    #[pyfn(m)]
    /// This function is for tests that require a value allocated on the heap.
    /// Should NOT be used in production.
    pub fn create_leaked_value(message: String) -> usize {
        let value = Value::Status(message);
        Box::leak(Box::new(value)) as *mut Value as usize
    }
    Ok(())
}

impl From<logger_core::Level> for Level {
    fn from(level: logger_core::Level) -> Self {
        match level {
            logger_core::Level::Error => Level::Error,
            logger_core::Level::Warn => Level::Warn,
            logger_core::Level::Info => Level::Info,
            logger_core::Level::Debug => Level::Debug,
            logger_core::Level::Trace => Level::Trace,
        }
    }
}

impl From<Level> for logger_core::Level {
    fn from(level: Level) -> logger_core::Level {
        match level {
            Level::Error => logger_core::Level::Error,
            Level::Warn => logger_core::Level::Warn,
            Level::Info => logger_core::Level::Info,
            Level::Debug => logger_core::Level::Debug,
            Level::Trace => logger_core::Level::Trace,
        }
    }
}

#[pyfunction]
pub fn log(log_level: Level, log_identifier: String, message: String) {
    logger_core::log(log_level.into(), log_identifier, message);
}

#[pyfunction]
pub fn init(level: Option<Level>, file_name: Option<&str>) -> Level {
    let logger_level = logger_core::init(level.map(|level| level.into()), file_name);
    logger_level.into()
}
