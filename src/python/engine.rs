//! Direct API: IbEngine and connect() function.

use std::sync::Arc;
use std::thread;

use crossbeam_channel::Sender;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::bridge::{BridgeStrategy, SharedState};
use crate::gateway::{Gateway, GatewayConfig};
use crate::types::*;
use super::types::*;

// ── Main engine class ──

#[pyclass]
pub struct IbEngine {
    shared: Arc<SharedState>,
    control_tx: Sender<ControlCommand>,
    next_order_id: std::sync::atomic::AtomicU64,
    _thread: Option<thread::JoinHandle<()>>,
    account_id: String,
}

#[pymethods]
impl IbEngine {
    fn subscribe(&self, conid: i64, symbol: String) -> PyResult<u32> {
        self.control_tx.send(ControlCommand::RegisterInstrument { con_id: conid })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        self.control_tx.send(ControlCommand::Subscribe { con_id: conid, symbol })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        std::thread::sleep(std::time::Duration::from_millis(10));
        Ok(self.shared.instrument_count().saturating_sub(1))
    }

    fn unsubscribe(&self, instrument: u32) -> PyResult<()> {
        self.control_tx.send(ControlCommand::Unsubscribe { instrument })
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    fn quote(&self, instrument: u32) -> PyQuote {
        self.shared.quote(instrument).into()
    }

    fn position(&self, instrument: u32) -> i64 {
        self.shared.position(instrument)
    }

    fn account(&self) -> PyAccountState {
        self.shared.account().into()
    }

    fn fills(&self) -> Vec<PyFill> {
        self.shared.drain_fills().into_iter().map(|f| f.into()).collect()
    }

    fn order_updates(&self) -> Vec<PyOrderUpdate> {
        self.shared.drain_order_updates().into_iter().map(|u| u.into()).collect()
    }

    fn submit_limit(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimit {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_market(&self, instrument: u32, side: &str, qty: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitMarket {
            order_id, instrument, side, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_stop(&self, instrument: u32, side: &str, qty: u32, stop_price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitStop {
            order_id, instrument, side, qty, stop_price: stop_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_stop_limit(&self, instrument: u32, side: &str, qty: u32, price: f64, stop_price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitStopLimit {
            order_id, instrument, side, qty, price: price_fixed, stop_price: stop_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_limit_gtc(&self, instrument: u32, side: &str, qty: u32, price: f64, outside_rth: bool) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitGtc {
            order_id, instrument, side, qty, price: price_fixed, outside_rth,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_stop_gtc(&self, instrument: u32, side: &str, qty: u32, stop_price: f64, outside_rth: bool) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitStopGtc {
            order_id, instrument, side, qty, stop_price: stop_fixed, outside_rth,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_stop_limit_gtc(&self, instrument: u32, side: &str, qty: u32, price: f64, stop_price: f64, outside_rth: bool) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitStopLimitGtc {
            order_id, instrument, side, qty, price: price_fixed, stop_price: stop_fixed, outside_rth,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_limit_ioc(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitIoc {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_limit_fok(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitFok {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_trailing_stop(&self, instrument: u32, side: &str, qty: u32, trail_amt: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let trail_fixed = (trail_amt * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitTrailingStop {
            order_id, instrument, side, qty, trail_amt: trail_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_trailing_stop_limit(&self, instrument: u32, side: &str, qty: u32, price: f64, trail_amt: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let trail_fixed = (trail_amt * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitTrailingStopLimit {
            order_id, instrument, side, qty, price: price_fixed, trail_amt: trail_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_trailing_stop_pct(&self, instrument: u32, side: &str, qty: u32, trail_pct: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitTrailingStopPct {
            order_id, instrument, side, qty, trail_pct,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_moc(&self, instrument: u32, side: &str, qty: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitMoc {
            order_id, instrument, side, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_loc(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLoc {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_mit(&self, instrument: u32, side: &str, qty: u32, stop_price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitMit {
            order_id, instrument, side, qty, stop_price: stop_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_lit(&self, instrument: u32, side: &str, qty: u32, price: f64, stop_price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLit {
            order_id, instrument, side, qty, price: price_fixed, stop_price: stop_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_bracket(&self, instrument: u32, side: &str, qty: u32, entry_price: f64, take_profit: f64, stop_loss: f64) -> PyResult<(u64, u64, u64)> {
        let parent_id = self.next_order_id.fetch_add(3, std::sync::atomic::Ordering::Relaxed);
        let tp_id = parent_id + 1;
        let sl_id = parent_id + 2;
        let side = parse_side(side)?;
        let entry_fixed = (entry_price * PRICE_SCALE_F) as i64;
        let tp_fixed = (take_profit * PRICE_SCALE_F) as i64;
        let sl_fixed = (stop_loss * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitBracket {
            parent_id, tp_id, sl_id, instrument, side, qty,
            entry_price: entry_fixed, take_profit: tp_fixed, stop_loss: sl_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok((parent_id, tp_id, sl_id))
    }

    #[pyo3(signature = (instrument, side, qty, price, tif="DAY", display_size=0, min_qty=0, hidden=false, outside_rth=false, good_after=0, good_till=0, oca_group=0))]
    fn submit_limit_ex(
        &self, instrument: u32, side: &str, qty: u32, price: f64,
        tif: &str, display_size: u32, min_qty: u32, hidden: bool, outside_rth: bool,
        good_after: i64, good_till: i64, oca_group: u64,
    ) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let tif_byte = match tif {
            "DAY" => b'0',
            "GTC" => b'1',
            "GTD" | "DTC" => b'6',
            _ => return Err(PyRuntimeError::new_err(format!("Invalid TIF: {}. Use DAY, GTC, GTD, or DTC", tif))),
        };
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitEx {
            order_id, instrument, side, qty, price: price_fixed, tif: tif_byte,
            attrs: OrderAttrs { display_size, min_qty, hidden, outside_rth, good_after, good_till, oca_group, ..OrderAttrs::default() },
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_rel(&self, instrument: u32, side: &str, qty: u32, offset: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let offset_fixed = (offset * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitRel {
            order_id, instrument, side, qty, offset: offset_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_limit_opg(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitOpg {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_adaptive(&self, instrument: u32, side: &str, qty: u32, price: f64, priority: &str) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let priority = match priority {
            "Patient" => AdaptivePriority::Patient,
            "Normal" => AdaptivePriority::Normal,
            "Urgent" => AdaptivePriority::Urgent,
            _ => return Err(PyRuntimeError::new_err(format!("Invalid priority: {}. Use Patient, Normal, or Urgent", priority))),
        };
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitAdaptive {
            order_id, instrument, side, qty, price: price_fixed, priority,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_mtl(&self, instrument: u32, side: &str, qty: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitMtl {
            order_id, instrument, side, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_mkt_prt(&self, instrument: u32, side: &str, qty: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitMktPrt {
            order_id, instrument, side, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_stp_prt(&self, instrument: u32, side: &str, qty: u32, stop_price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let stop_fixed = (stop_price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitStpPrt {
            order_id, instrument, side, qty, stop_price: stop_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    #[pyo3(signature = (instrument, side, qty, price_cap=0.0))]
    fn submit_mid_price(&self, instrument: u32, side: &str, qty: u32, price_cap: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let cap_fixed = (price_cap * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitMidPrice {
            order_id, instrument, side, qty, price_cap: cap_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_snap_mkt(&self, instrument: u32, side: &str, qty: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitSnapMkt {
            order_id, instrument, side, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_snap_mid(&self, instrument: u32, side: &str, qty: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitSnapMid {
            order_id, instrument, side, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_snap_pri(&self, instrument: u32, side: &str, qty: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitSnapPri {
            order_id, instrument, side, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    #[pyo3(signature = (instrument, side, qty, offset=0.0))]
    fn submit_peg_mkt(&self, instrument: u32, side: &str, qty: u32, offset: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let offset_fixed = (offset * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitPegMkt {
            order_id, instrument, side, qty, offset: offset_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    #[pyo3(signature = (instrument, side, qty, offset=0.0))]
    fn submit_peg_mid(&self, instrument: u32, side: &str, qty: u32, offset: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let offset_fixed = (offset * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitPegMid {
            order_id, instrument, side, qty, offset: offset_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_peg_bench(
        &self, instrument: u32, side: &str, qty: u32, price: f64,
        ref_con_id: u32, is_peg_decrease: bool, pegged_change_amount: f64, ref_change_amount: f64,
    ) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let peg_change_fixed = (pegged_change_amount * PRICE_SCALE_F) as i64;
        let ref_change_fixed = (ref_change_amount * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitPegBench {
            order_id, instrument, side, qty, price: price_fixed,
            ref_con_id, is_peg_decrease,
            pegged_change_amount: peg_change_fixed,
            ref_change_amount: ref_change_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_limit_auc(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitAuc {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_mtl_auc(&self, instrument: u32, side: &str, qty: u32) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitMtlAuc {
            order_id, instrument, side, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_box_top(&self, instrument: u32, side: &str, qty: u32) -> PyResult<u64> {
        self.submit_mtl(instrument, side, qty)
    }

    fn submit_what_if(&self, instrument: u32, side: &str, qty: u32, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitWhatIf {
            order_id, instrument, side, qty, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn submit_limit_fractional(&self, instrument: u32, side: &str, qty: f64, price: f64) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        let qty_fixed = (qty * QTY_SCALE as f64) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitLimitFractional {
            order_id, instrument, side, qty: qty_fixed, price: price_fixed,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    #[pyo3(signature = (instrument, side, qty, stop_price, trigger_price, adjusted_type, adjusted_stop_price, adjusted_limit_price=0.0))]
    fn submit_adjustable_stop(
        &self, instrument: u32, side: &str, qty: u32, stop_price: f64,
        trigger_price: f64, adjusted_type: &str, adjusted_stop_price: f64,
        adjusted_limit_price: f64,
    ) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let adjusted_order_type = match adjusted_type {
            "Stop" => AdjustedOrderType::Stop,
            "StopLimit" => AdjustedOrderType::StopLimit,
            "Trail" => AdjustedOrderType::Trail,
            "TrailLimit" => AdjustedOrderType::TrailLimit,
            _ => return Err(PyRuntimeError::new_err(format!("Invalid adjusted_type '{}': use Stop, StopLimit, Trail, or TrailLimit", adjusted_type))),
        };
        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitAdjustableStop {
            order_id, instrument, side, qty,
            stop_price: (stop_price * PRICE_SCALE_F) as i64,
            trigger_price: (trigger_price * PRICE_SCALE_F) as i64,
            adjusted_order_type,
            adjusted_stop_price: (adjusted_stop_price * PRICE_SCALE_F) as i64,
            adjusted_stop_limit_price: (adjusted_limit_price * PRICE_SCALE_F) as i64,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    /// Submit an algo order (VWAP, TWAP, ArrivalPx, ClosePx, DarkIce, PctVol).
    fn submit_algo(
        &self, instrument: u32, side: &str, qty: u32, price: f64,
        algo: &str, max_pct_vol: f64, start_time: String, end_time: String,
    ) -> PyResult<u64> {
        let order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let side = parse_side(side)?;
        let price_fixed = (price * PRICE_SCALE_F) as i64;

        let algo_params = match algo.to_lowercase().as_str() {
            "vwap" => AlgoParams::Vwap {
                max_pct_vol, no_take_liq: false, allow_past_end_time: false,
                start_time, end_time,
            },
            "twap" => AlgoParams::Twap {
                allow_past_end_time: false, start_time, end_time,
            },
            "arrivalpx" => AlgoParams::ArrivalPx {
                max_pct_vol, risk_aversion: RiskAversion::Neutral,
                allow_past_end_time: false, force_completion: false,
                start_time, end_time,
            },
            "closepx" => AlgoParams::ClosePx {
                max_pct_vol, risk_aversion: RiskAversion::Neutral,
                force_completion: false, start_time,
            },
            "darkice" => AlgoParams::DarkIce {
                allow_past_end_time: false, display_size: 100,
                start_time, end_time,
            },
            "pctvol" => AlgoParams::PctVol {
                pct_vol: max_pct_vol, no_take_liq: false,
                start_time, end_time,
            },
            _ => return Err(PyRuntimeError::new_err(format!("Unknown algo: '{}'. Use vwap, twap, arrivalpx, closepx, darkice, pctvol", algo))),
        };

        self.control_tx.send(ControlCommand::Order(OrderRequest::SubmitAlgo {
            order_id, instrument, side, qty, price: price_fixed, algo: algo_params,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(order_id)
    }

    fn cancel(&self, order_id: u64) -> PyResult<()> {
        self.control_tx.send(ControlCommand::Order(OrderRequest::Cancel { order_id }))
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    fn cancel_all(&self, instrument: u32) -> PyResult<()> {
        self.control_tx.send(ControlCommand::Order(OrderRequest::CancelAll { instrument }))
            .map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(())
    }

    fn modify(&self, order_id: u64, price: f64, qty: u32) -> PyResult<u64> {
        let new_order_id = self.next_order_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let price_fixed = (price * PRICE_SCALE_F) as i64;
        self.control_tx.send(ControlCommand::Order(OrderRequest::Modify {
            new_order_id, order_id, price: price_fixed, qty,
        })).map_err(|e| PyRuntimeError::new_err(format!("Engine stopped: {}", e)))?;
        Ok(new_order_id)
    }

    /// Drain all pending cancel/modify rejects.
    fn cancel_rejects(&self) -> Vec<PyCancelReject> {
        self.shared.drain_cancel_rejects().into_iter().map(|r| r.into()).collect()
    }

    fn shutdown(&self) -> PyResult<()> {
        self.control_tx.send(ControlCommand::Shutdown)
            .map_err(|e| PyRuntimeError::new_err(format!("Engine already stopped: {}", e)))?;
        Ok(())
    }

    #[getter]
    fn account_id(&self) -> &str {
        &self.account_id
    }
}

/// Python-visible cancel reject type.
#[pyclass]
#[derive(Clone)]
pub struct PyCancelReject {
    #[pyo3(get)]
    pub order_id: u64,
    #[pyo3(get)]
    pub reject_type: u8,
    #[pyo3(get)]
    pub reason_code: i32,
    #[pyo3(get)]
    pub timestamp_ns: u64,
}

#[pymethods]
impl PyCancelReject {
    fn __repr__(&self) -> String {
        format!("CancelReject(order_id={}, reason={})", self.order_id, self.reason_code)
    }
}

impl From<CancelReject> for PyCancelReject {
    fn from(r: CancelReject) -> Self {
        Self {
            order_id: r.order_id,
            reject_type: r.reject_type,
            reason_code: r.reason_code,
            timestamp_ns: r.timestamp_ns,
        }
    }
}

// ── Module-level connect function ──

#[pyfunction]
#[pyo3(signature = (username, password, paper=true, host="cdc1.ibllc.com".to_string(), core_id=None))]
pub fn connect(
    py: Python<'_>,
    username: String,
    password: String,
    paper: bool,
    host: String,
    core_id: Option<usize>,
) -> PyResult<IbEngine> {
    let config = GatewayConfig { username, password, host, paper };

    let result = py.allow_threads(|| Gateway::connect(&config));
    let (gw, farm_conn, ccp_conn, hmds_conn) = result
        .map_err(|e| PyRuntimeError::new_err(format!("Connection failed: {}", e)))?;

    let account_id = gw.account_id.clone();
    let shared = Arc::new(SharedState::new());
    let shared_clone = shared.clone();
    let strategy = BridgeStrategy::new(shared_clone);

    let (mut hot_loop, control_tx) = gw.into_hot_loop(strategy, farm_conn, ccp_conn, hmds_conn, core_id);

    let start_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() * 1000;

    let handle = thread::Builder::new()
        .name("ib-engine-hotloop".into())
        .spawn(move || {
            hot_loop.run();
        })
        .map_err(|e| PyRuntimeError::new_err(format!("Failed to spawn hot loop thread: {}", e)))?;

    Ok(IbEngine {
        shared,
        control_tx,
        next_order_id: std::sync::atomic::AtomicU64::new(start_id),
        _thread: Some(handle),
        account_id,
    })
}
