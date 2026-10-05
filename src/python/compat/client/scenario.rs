//! Scenario replay through the Python client (ibx#487, `test-support`
//! feature only): the recorded scenario of `test_support::scenario` drives
//! this client, a Python driver makes each request and hands back the
//! callbacks its wrapper got, in the official client library's form.

use std::sync::atomic::Ordering;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::test_support::scenario::runner::{run, Driver, Options};
use crate::test_support::scenario::{canonical, load_codec, load_scenario, Links, Rec};

use super::EClient;

/// The Python driver: `request(name, request_json) -> bool` makes the
/// recorded request on the client; `dispatch() -> str` runs the client's
/// dispatch once and gives the wrapper calls since the last time, as a JSON
/// list of `[name, arg, ...]` in the official client library's form.
struct PyDriver<'py> {
    client: &'py EClient,
    driver: Bound<'py, PyAny>,
    error: Option<PyErr>,
}

impl Driver for PyDriver<'_> {
    fn start(&mut self, client_id: i64) {
        self.client.core.client_id.store(client_id, Ordering::Relaxed);
    }

    fn request(&mut self, links: &mut Links, r: &Rec) -> bool {
        let request = r.request.to_string();
        let (driver, name) = (&self.driver, r.msg.as_str());
        match links.during(|| driver.call_method1("request", (name, request.as_str()))) {
            Ok(v) => v.extract::<bool>().unwrap_or(false),
            Err(e) => {
                self.error.get_or_insert(e);
                false
            }
        }
    }

    fn dispatch(&mut self) -> Vec<String> {
        let calls = match self.driver.call_method0("dispatch").and_then(|v| v.extract::<String>()) {
            Ok(text) => text,
            Err(e) => {
                self.error.get_or_insert(e);
                return Vec::new();
            }
        };
        let calls: serde_json::Value = serde_json::from_str(&calls).unwrap_or_default();
        calls.as_array().into_iter().flatten().filter_map(canonical).collect()
    }
}

#[pymethods]
impl EClient {
    /// Replay a recorded scenario through this client (test-only): the
    /// client is attached to an engine on in-memory links, `driver` makes
    /// the requests and gives the callbacks. `name` is a scenario
    /// (`20260926/lmt_cancel`) or, with `codec=True`, a codec fixture.
    /// Options: `until`, `skip_seqs`, `skip_orders`, `compare` (frame kinds),
    /// `farms` (other market data farms played on the farm link).
    /// Returns a dict: `frame_error`, `frames_compared`, `ours`, `theirs`
    /// (callback lines), `unsent`, `not_made`.
    #[doc(hidden)]
    #[pyo3(signature = (name, driver, codec=false, until=None, skip_seqs=Vec::new(), skip_orders=Vec::new(), compare=None, farms=Vec::new()))]
    #[allow(clippy::too_many_arguments)]
    fn _test_replay_scenario<'py>(
        &self, py: Python<'py>, name: &str, driver: Bound<'py, PyAny>, codec: bool, until: Option<u64>,
        skip_seqs: Vec<u64>, skip_orders: Vec<i64>, compare: Option<Vec<String>>, farms: Vec<String>,
    ) -> PyResult<Bound<'py, PyDict>> {
        if self.connected.load(Ordering::Acquire) {
            return Err(PyRuntimeError::new_err("Already connected"));
        }
        let scenario = if codec { load_codec(name) } else { load_scenario(name) };
        let mut opts = Options::default().skip_seqs(&skip_seqs).skip_orders(&skip_orders);
        if !farms.is_empty() {
            // Test-only: the farm names live as long as the process.
            let farms: Vec<&'static str> = farms.into_iter().map(|f| &*Box::leak(f.into_boxed_str())).collect();
            opts = opts.farms(&farms);
        }
        if let Some(u) = until {
            opts = opts.until(u);
        }
        if let Some(kinds) = compare {
            let known = [
                crate::test_support::scenario::runner::ORDER, crate::test_support::scenario::runner::LOOKUP,
                crate::test_support::scenario::runner::MARKET_DATA, crate::test_support::scenario::runner::SUBSCRIPTION,
                crate::test_support::scenario::runner::HISTORICAL, crate::test_support::scenario::runner::SCANNER,
            ];
            let kinds: Vec<&'static str> = known.iter().copied().filter(|k| kinds.iter().any(|w| w == k)).collect();
            opts = opts.compare(&kinds);
        }
        // The replay runs in the recording's machine zone (`run`); this
        // thread gets its own zone back after it.
        let zone_before = crate::gateway::machine_zone_for_test();
        let mut links = Links::new();
        // Attached as a connected client of this engine.
        *self.shared.lock().unwrap() = Some(links.shared.clone());
        *self.control_tx.lock().unwrap() = Some(links.control_tx.clone());
        *self.account_id.lock().unwrap() = Some(crate::test_support::scenario::session::ACCOUNT.to_string());
        *self.connection_time.lock().unwrap() = Some(crate::client_core::connection_time_now());
        self.connected.store(true, Ordering::Release);
        let mut d = PyDriver { client: self, driver, error: None };
        let outcome = run(&scenario, &opts, &mut links, &mut d);
        crate::gateway::set_machine_zone_for_test(zone_before.as_deref());
        if let Some(e) = d.error.take() {
            return Err(e);
        }
        let out = PyDict::new(py);
        out.set_item("frame_error", outcome.frame_error)?;
        out.set_item("frames_compared", outcome.frames_compared)?;
        out.set_item("ours", outcome.ours)?;
        out.set_item("theirs", outcome.theirs.into_iter().map(|(_, l)| l).collect::<Vec<_>>())?;
        out.set_item("unsent", outcome.unsent)?;
        out.set_item("not_made", outcome.not_made)?;
        Ok(out)
    }
}
