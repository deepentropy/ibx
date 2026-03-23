//! Event dispatch: drains SharedState queues and fires Python wrapper callbacks.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pyo3::prelude::*;

use crate::bridge::SharedState;
use crate::client_core::order_status_str;
use crate::types::*;

use super::{EClient, StoredExecution};
use super::super::contract::{Contract, ContractDescription, ContractDetails, BarData, CommissionReport};
use super::super::tick_types::*;
use super::super::super::types::PRICE_SCALE_F;

impl EClient {
    /// Single iteration of event dispatch: drain all shared queues and fire Python callbacks.
    pub(crate) fn dispatch_once(&self, py: Python<'_>, shared: &Arc<SharedState>) -> PyResult<()> {
        // Drain fills -> execDetails + orderStatus
        let fills = shared.orders.drain_fills();
        for fill in fills {
            let req_id = self.core.instrument_to_req.lock().unwrap()
                .get(&fill.instrument).copied().unwrap_or(-1);
            let side_str = match fill.side {
                Side::Buy => "BUY",
                Side::Sell => "SELL",
                Side::ShortSell => "SSHORT",
            };
            let price = fill.price as f64 / PRICE_SCALE_F;
            let commission = fill.commission as f64 / PRICE_SCALE_F;

            let status = if fill.remaining == 0 { "Filled" } else { "PartiallyFilled" };
            self.wrapper.call_method(
                py, "order_status",
                (fill.order_id as i64, status, fill.qty as f64, fill.remaining as f64,
                 price, 0i64, 0i64, price, 0i64, "", 0.0f64),
                None,
            )?;

            // Track execution for req_executions
            let exec_id = format!("{}.{}", fill.order_id, fill.timestamp_ns);
            let now_str = format!("{}", fill.timestamp_ns);
            let rich_info = shared.orders.get_order_info(fill.order_id);
            let exec_exchange = rich_info.as_ref()
                .map(|i| i.last_exec.exchange.as_str()).unwrap_or("").to_string();
            let cum_qty = rich_info.as_ref()
                .map(|i| i.last_exec.cum_qty).unwrap_or(fill.qty as f64);
            let avg_price = rich_info.as_ref()
                .map(|i| i.last_exec.avg_price).unwrap_or(price);
            let exec_contract = self.open_orders.lock().unwrap()
                .get(&fill.order_id).map(|so| so.contract.clone())
                .or_else(|| {
                    rich_info.map(|info| {
                        Contract {
                            con_id: info.contract.con_id,
                            symbol: info.contract.symbol,
                            sec_type: info.contract.sec_type,
                            exchange: info.contract.exchange,
                            currency: info.contract.currency,
                            ..Default::default()
                        }
                    })
                })
                .unwrap_or_default();
            self.executions.lock().unwrap().push(StoredExecution {
                req_id,
                contract: exec_contract.clone(),
                exec_id: exec_id.clone(),
                side: side_str.to_string(),
                price,
                shares: fill.qty as f64,
                time: now_str.clone(),
                order_id: fill.order_id,
                cum_qty,
                avg_price,
                exchange: exec_exchange.clone(),
                commission,
            });

            let acct_name = self.account();
            let c_py = Py::new(py, exec_contract)?.into_any();
            let exec_dict = pyo3::types::PyDict::new(py);
            exec_dict.set_item("execId", exec_id.as_str())?;
            exec_dict.set_item("time", now_str.as_str())?;
            exec_dict.set_item("acctNumber", acct_name.as_str())?;
            exec_dict.set_item("exchange", exec_exchange.as_str())?;
            exec_dict.set_item("side", side_str)?;
            exec_dict.set_item("price", price)?;
            exec_dict.set_item("shares", fill.qty as f64)?;
            exec_dict.set_item("orderId", fill.order_id as i64)?;
            exec_dict.set_item("cumQty", cum_qty)?;
            exec_dict.set_item("avgPrice", avg_price)?;
            exec_dict.set_item("permId", 0i64)?;
            exec_dict.set_item("clientId", 0i64)?;
            exec_dict.set_item("liquidation", 0i64)?;
            exec_dict.set_item("lastLiquidity", 0i64)?;
            exec_dict.set_item("pendingPriceRevision", false)?;
            self.wrapper.call_method(
                py, "exec_details",
                (req_id, &c_py, exec_dict.as_any()),
                None,
            )?;

            // Update open order tracking
            {
                let mut orders = self.open_orders.lock().unwrap();
                if fill.remaining == 0 {
                    orders.remove(&fill.order_id);
                } else if let Some(so) = orders.get_mut(&fill.order_id) {
                    so.status = status.to_string();
                    so.filled = fill.qty as f64;
                    so.remaining = fill.remaining as f64;
                }
            }

            // Dispatch commission_report
            let report = CommissionReport {
                exec_id,
                commission,
                currency: "USD".to_string(),
                realized_pnl: f64::MAX,
                yield_amount: f64::MAX,
                yield_redemption_date: String::new(),
            };
            let report_py = Py::new(py, report)?.into_any();
            self.wrapper.call_method1(py, "commission_report", (&report_py,))?;
        }

        // Drain order updates -> orderStatus
        let updates = shared.orders.drain_order_updates();
        for update in updates {
            let status = order_status_str(update.status);
            self.wrapper.call_method(
                py, "order_status",
                (update.order_id as i64, status, update.filled_qty as f64,
                 update.remaining_qty as f64, 0.0f64, 0i64, 0i64, 0.0f64, 0i64, "", 0.0f64),
                None,
            )?;

            // Track open orders
            {
                let mut orders = self.open_orders.lock().unwrap();
                match update.status {
                    OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected => {
                        if let Some(so) = orders.get_mut(&update.order_id) {
                            so.status = status.to_string();
                            so.filled = update.filled_qty as f64;
                            so.remaining = update.remaining_qty as f64;
                        }
                    }
                    _ => {
                        if let Some(so) = orders.get_mut(&update.order_id) {
                            so.status = status.to_string();
                            so.filled = update.filled_qty as f64;
                            so.remaining = update.remaining_qty as f64;
                        }
                    }
                }
            }
        }

        // Drain cancel rejects -> error
        let rejects = shared.orders.drain_cancel_rejects();
        for reject in rejects {
            let code = if reject.reject_type == 1 { 202i64 } else { 10147i64 };
            let msg = format!("Order {} cancel/modify rejected (reason: {})", reject.order_id, reject.reason_code);
            self.wrapper.call_method(
                py, "error",
                (reject.order_id as i64, code, msg.as_str(), ""),
                None,
            )?;
        }

        // Poll quotes for changes -> tickPrice/tickSize
        let instruments: Vec<(u32, i64)> = {
            let map = self.core.instrument_to_req.lock().unwrap();
            map.iter().map(|(&iid, &req_id)| (iid, req_id)).collect()
        };
        let mut snapshot_done: Vec<i64> = Vec::new();
        for (iid, req_id) in instruments {
            let q = shared.market.quote(iid);
            let fields = [
                q.bid, q.ask, q.last, q.bid_size, q.ask_size, q.last_size,
                q.high, q.low, q.volume, q.close, q.open, q.timestamp_ns as i64,
            ];

            let last = {
                let map = self.core.last_quotes.lock().unwrap();
                map.get(&iid).copied().unwrap_or([0i64; 12])
            };

            let attrib = TickAttrib::default();
            let attrib_obj = Py::new(py, attrib)?.into_any();

            // Fire market_data_type once per subscription on first tick delivery
            let any_data = fields.iter().any(|&f| f != 0);
            if any_data && self.mdt_sent.lock().unwrap().insert(req_id) {
                let mdt = self.market_data_type.load(Ordering::Relaxed);
                self.wrapper.call_method1(py, "market_data_type", (req_id, mdt))?;
            }

            let mut delivered = false;
            if fields[0] != last[0] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_BID, fields[0] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[1] != last[1] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_ASK, fields[1] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[2] != last[2] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_LAST, fields[2] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[3] != last[3] {
                self.wrapper.call_method1(py, "tick_size", (req_id, TICK_BID_SIZE, fields[3] as f64 / QTY_SCALE as f64))?;
                delivered = true;
            }
            if fields[4] != last[4] {
                self.wrapper.call_method1(py, "tick_size", (req_id, TICK_ASK_SIZE, fields[4] as f64 / QTY_SCALE as f64))?;
                delivered = true;
            }
            if fields[5] != last[5] {
                self.wrapper.call_method1(py, "tick_size", (req_id, TICK_LAST_SIZE, fields[5] as f64 / QTY_SCALE as f64))?;
                delivered = true;
            }
            if fields[6] != last[6] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_HIGH, fields[6] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[7] != last[7] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_LOW, fields[7] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[8] != last[8] {
                self.wrapper.call_method1(py, "tick_size", (req_id, TICK_VOLUME, fields[8] as f64 / QTY_SCALE as f64))?;
                delivered = true;
            }
            if fields[9] != last[9] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_CLOSE, fields[9] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            if fields[10] != last[10] {
                self.wrapper.call_method1(py, "tick_price", (req_id, TICK_OPEN, fields[10] as f64 / PRICE_SCALE_F, &attrib_obj))?;
                delivered = true;
            }
            // tick_string: LAST_TIMESTAMP as epoch seconds
            if fields[11] != last[11] && fields[11] != 0 {
                let ts_secs = fields[11] / 1_000_000_000;
                let ts_str = ts_secs.to_string();
                self.wrapper.call_method1(py, "tick_string", (req_id, TICK_LAST_TIMESTAMP, ts_str.as_str()))?;
            }

            self.core.last_quotes.lock().unwrap().insert(iid, fields);

            // Snapshot: after first delivery, signal end and queue for auto-cancel
            if delivered && self.core.snapshot_reqs.lock().unwrap().remove(&req_id) {
                self.wrapper.call_method1(py, "tick_snapshot_end", (req_id,))?;
                snapshot_done.push(req_id);
            }
        }
        // Auto-cancel completed snapshots
        for req_id in snapshot_done {
            self.cancel_mkt_data(req_id)?;
        }

        // Drain TBT trades -> tickByTickAllLast
        let tbt_trades = shared.market.drain_tbt_trades();
        for trade in tbt_trades {
            let req_id = self.core.instrument_to_req.lock().unwrap()
                .get(&trade.instrument).copied().unwrap_or(-1);
            let price = trade.price as f64 / PRICE_SCALE_F;
            let size = trade.size as f64;
            let attrib = super::super::tick_types::TickAttribLast::default();
            let attrib_obj = Py::new(py, attrib)?.into_any();
            self.wrapper.call_method(
                py, "tick_by_tick_all_last",
                (req_id, 1i32, trade.timestamp as i64, price, size,
                 &attrib_obj, trade.exchange.as_str(), trade.conditions.as_str()),
                None,
            )?;
        }

        // Drain TBT quotes -> tickByTickBidAsk
        let tbt_quotes = shared.market.drain_tbt_quotes();
        for quote in tbt_quotes {
            let req_id = self.core.instrument_to_req.lock().unwrap()
                .get(&quote.instrument).copied().unwrap_or(-1);
            let attrib = super::super::tick_types::TickAttribBidAsk::default();
            let attrib_obj = Py::new(py, attrib)?.into_any();
            self.wrapper.call_method(
                py, "tick_by_tick_bid_ask",
                (req_id, quote.timestamp as i64,
                 quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                 quote.bid_size as f64, quote.ask_size as f64, &attrib_obj),
                None,
            )?;
        }

        // Drain depth updates -> updateMktDepth / updateMktDepthL2
        let depth_updates = shared.market.drain_depth_updates();
        for du in depth_updates {
            if du.market_maker.is_empty() {
                self.wrapper.call_method(
                    py, "update_mkt_depth",
                    (du.req_id as i64, du.position, du.operation, du.side, du.price, du.size),
                    None,
                )?;
            } else {
                self.wrapper.call_method(
                    py, "update_mkt_depth_l2",
                    (du.req_id as i64, du.position, du.market_maker.as_str(),
                     du.operation, du.side, du.price, du.size, du.is_smart_depth),
                    None,
                )?;
            }
        }

        // Drain news -> tickNews
        let news_items = shared.market.drain_tick_news();
        for news in news_items {
            let first_req_id = self.core.instrument_to_req.lock().unwrap()
                .values().next().copied();
            if let Some(req_id) = first_req_id {
                self.wrapper.call_method(
                    py, "tick_news",
                    (req_id, news.timestamp as i64, news.provider_code.as_str(),
                     news.article_id.as_str(), news.headline.as_str(), ""),
                    None,
                )?;
            }
        }

        // Drain news bulletins -> updateNewsBulletin
        if self.core.bulletin_subscribed.load(Ordering::Acquire) {
            let bulletins = shared.market.drain_news_bulletins();
            for b in bulletins {
                self.wrapper.call_method(
                    py, "update_news_bulletin",
                    (b.msg_id as i64, b.msg_type, b.message.as_str(), b.exchange.as_str()),
                    None,
                )?;
            }
        }

        // Drain what-if responses -> orderStatus with margin info
        let what_ifs = shared.orders.drain_what_if_responses();
        for wi in what_ifs {
            let msg = format!(
                "WhatIf: initMargin={:.2}, maintMargin={:.2}, commission={:.2}",
                wi.init_margin_after as f64 / PRICE_SCALE_F,
                wi.maint_margin_after as f64 / PRICE_SCALE_F,
                wi.commission as f64 / PRICE_SCALE_F,
            );
            self.wrapper.call_method(
                py, "order_status",
                (wi.order_id as i64, "PreSubmitted", 0.0f64, 0.0f64,
                 0.0f64, 0i64, 0i64, 0.0f64, 0i64, msg.as_str(), 0.0f64),
                None,
            )?;
        }

        // Drain historical data -> historicalData + historicalDataEnd
        let hist_data = shared.reference.drain_historical_data();
        for (req_id, response) in hist_data {
            for bar in &response.bars {
                let bar_obj = BarData::new(
                    bar.time.clone(), bar.open, bar.high, bar.low, bar.close,
                    bar.volume, bar.wap, bar.count as i32,
                );
                let bar_py = Py::new(py, bar_obj)?.into_any();
                self.wrapper.call_method1(py, "historical_data", (req_id as i64, &bar_py))?;
            }
            if response.is_complete {
                self.wrapper.call_method(
                    py, "historical_data_end",
                    (req_id as i64, "", ""),
                    None,
                )?;
            }
        }

        // Drain head timestamps -> headTimestamp
        let head_ts = shared.reference.drain_head_timestamps();
        for (req_id, response) in head_ts {
            self.wrapper.call_method1(
                py, "head_timestamp",
                (req_id as i64, response.head_timestamp.as_str()),
            )?;
        }

        // Drain contract details -> contractDetails + contractDetailsEnd
        let contract_defs = shared.reference.drain_contract_details();
        for (req_id, def) in contract_defs {
            let details = ContractDetails::from_definition(&def);
            let details_py = Py::new(py, details)?.into_any();
            self.wrapper.call_method1(
                py, "contract_details",
                (req_id as i64, &details_py),
            )?;
        }
        let contract_ends = shared.reference.drain_contract_details_end();
        for req_id in contract_ends {
            self.wrapper.call_method1(py, "contract_details_end", (req_id as i64,))?;
        }

        // Drain matching symbols -> symbolSamples
        let symbol_results = shared.reference.drain_matching_symbols();
        for (req_id, matches) in symbol_results {
            let descriptions: Vec<Py<ContractDescription>> = matches.iter().map(|m| {
                Py::new(py, ContractDescription {
                    con_id: m.con_id as i64,
                    symbol: m.symbol.clone(),
                    sec_type: m.sec_type.to_fix().to_string(),
                    currency: m.currency.clone(),
                    primary_exchange: m.primary_exchange.clone(),
                    derivative_sec_types: m.derivative_types.clone(),
                }).unwrap()
            }).collect();
            let list = pyo3::types::PyList::new(py, &descriptions)?;
            self.wrapper.call_method1(py, "symbol_samples", (req_id as i64, list.as_any()))?;
        }

        // Drain scanner params -> scannerParameters
        let scanner_params = shared.reference.drain_scanner_params();
        for xml in scanner_params {
            self.wrapper.call_method1(py, "scanner_parameters", (xml.as_str(),))?;
        }

        // Drain scanner data -> scannerData + scannerDataEnd
        let scanner_results = shared.reference.drain_scanner_data();
        for (req_id, result) in scanner_results {
            for (rank, &con_id) in result.con_ids.iter().enumerate() {
                let mut cd = ContractDetails::default();
                cd.contract.con_id = con_id as i64;
                let cd_py = Py::new(py, cd)?.into_any();
                self.wrapper.call_method(
                    py, "scanner_data",
                    (req_id as i64, rank as i32, &cd_py, "", "", "", ""),
                    None,
                )?;
            }
            self.wrapper.call_method1(py, "scanner_data_end", (req_id as i64,))?;
        }

        // Drain historical news -> historicalNews + historicalNewsEnd
        let news_results = shared.reference.drain_historical_news();
        for (req_id, headlines, has_more) in news_results {
            for h in &headlines {
                self.wrapper.call_method(
                    py, "historical_news",
                    (req_id as i64, h.time.as_str(), h.provider_code.as_str(),
                     h.article_id.as_str(), h.headline.as_str()),
                    None,
                )?;
            }
            self.wrapper.call_method1(py, "historical_news_end", (req_id as i64, has_more))?;
        }

        // Drain news articles -> newsArticle
        let articles = shared.reference.drain_news_articles();
        for (req_id, article_type, text) in articles {
            self.wrapper.call_method(
                py, "news_article",
                (req_id as i64, article_type, text.as_str()),
                None,
            )?;
        }

        // Drain fundamental data -> fundamentalData
        let fundamentals = shared.reference.drain_fundamental_data();
        for (req_id, data) in fundamentals {
            self.wrapper.call_method1(py, "fundamental_data", (req_id as i64, data.as_str()))?;
        }

        // Drain histogram data -> histogram_data
        let histograms = shared.reference.drain_histogram_data();
        for (req_id, entries) in histograms {
            let tuples: Vec<Bound<'_, pyo3::types::PyTuple>> = entries.iter().map(|e| {
                pyo3::types::PyTuple::new(py, &[e.price.into_pyobject(py).unwrap().into_any(), e.count.into_pyobject(py).unwrap().into_any()]).unwrap()
            }).collect();
            let py_list = pyo3::types::PyList::new(py, tuples)?;
            self.wrapper.call_method1(py, "histogram_data", (req_id as i64, py_list))?;
        }

        // Drain historical ticks
        let hist_ticks = shared.reference.drain_historical_ticks();
        for (req_id, data, _what, done) in hist_ticks {
            match data {
                crate::types::HistoricalTickData::Midpoint(ticks) => {
                    let py_ticks: Vec<Bound<'_, pyo3::types::PyTuple>> = ticks.iter().map(|t| {
                        pyo3::types::PyTuple::new(py, &[
                            t.time.as_str().into_pyobject(py).unwrap().into_any(),
                            t.price.into_pyobject(py).unwrap().into_any(),
                        ]).unwrap()
                    }).collect();
                    let list = pyo3::types::PyList::new(py, py_ticks)?;
                    self.wrapper.call_method1(py, "historical_ticks", (req_id as i64, list, done))?;
                }
                crate::types::HistoricalTickData::Last(ticks) => {
                    let py_ticks: Vec<Bound<'_, pyo3::types::PyTuple>> = ticks.iter().map(|t| {
                        pyo3::types::PyTuple::new(py, &[
                            t.time.as_str().into_pyobject(py).unwrap().into_any(),
                            t.price.into_pyobject(py).unwrap().into_any(),
                            t.size.into_pyobject(py).unwrap().into_any(),
                            t.exchange.as_str().into_pyobject(py).unwrap().into_any(),
                            t.special_conditions.as_str().into_pyobject(py).unwrap().into_any(),
                        ]).unwrap()
                    }).collect();
                    let list = pyo3::types::PyList::new(py, py_ticks)?;
                    self.wrapper.call_method1(py, "historical_ticks_last", (req_id as i64, list, done))?;
                }
                crate::types::HistoricalTickData::BidAsk(ticks) => {
                    let py_ticks: Vec<Bound<'_, pyo3::types::PyTuple>> = ticks.iter().map(|t| {
                        pyo3::types::PyTuple::new(py, &[
                            t.time.as_str().into_pyobject(py).unwrap().into_any(),
                            t.bid_price.into_pyobject(py).unwrap().into_any(),
                            t.ask_price.into_pyobject(py).unwrap().into_any(),
                            t.bid_size.into_pyobject(py).unwrap().into_any(),
                            t.ask_size.into_pyobject(py).unwrap().into_any(),
                        ]).unwrap()
                    }).collect();
                    let list = pyo3::types::PyList::new(py, py_ticks)?;
                    self.wrapper.call_method1(py, "historical_ticks_bid_ask", (req_id as i64, list, done))?;
                }
            }
        }

        // Drain real-time bars -> real_time_bar
        let rtbars = shared.market.drain_real_time_bars();
        for (req_id, bar) in rtbars {
            self.wrapper.call_method1(py, "real_time_bar", (
                req_id as i64,
                bar.timestamp as i64,
                bar.open, bar.high, bar.low, bar.close,
                bar.volume, bar.wap, bar.count,
            ))?;
        }

        // Drain historical schedules -> historical_schedule
        let schedules = shared.reference.drain_historical_schedules();
        for (req_id, resp) in schedules {
            let sessions: Vec<Bound<'_, pyo3::types::PyTuple>> = resp.sessions.iter().map(|s| {
                pyo3::types::PyTuple::new(py, &[
                    s.ref_date.as_str().into_pyobject(py).unwrap().into_any(),
                    s.open_time.as_str().into_pyobject(py).unwrap().into_any(),
                    s.close_time.as_str().into_pyobject(py).unwrap().into_any(),
                ]).unwrap()
            }).collect();
            let py_sessions = pyo3::types::PyList::new(py, sessions)?;
            self.wrapper.call_method1(py, "historical_schedule", (
                req_id as i64,
                resp.start_date_time.as_str(),
                resp.end_date_time.as_str(),
                resp.timezone.as_str(),
                py_sessions,
            ))?;
        }

        // Account updates (via ClientCore)
        if let Some(batch) = self.core.prepare_account_updates(shared) {
            let account_name = self.account();
            for field in &batch.fields {
                self.wrapper.call_method1(py, "update_account_value",
                    (field.key, field.value.as_str(), field.currency, account_name.as_str()))?;
            }
            if batch.delivered {
                self.wrapper.call_method1(py, "update_account_time", ("",))?;
                self.wrapper.call_method1(py, "account_download_end", (account_name.as_str(),))?;
            }
        }

        // P&L dispatch (via ClientCore)
        if let Some(update) = self.core.poll_pnl(shared) {
            self.wrapper.call_method(
                py, "pnl",
                (update.req_id, update.daily_pnl, update.unrealized_pnl, update.realized_pnl),
                None,
            )?;
        }

        // Per-position P&L dispatch (via ClientCore)
        for update in self.core.poll_pnl_single(shared) {
            self.wrapper.call_method(
                py, "pnl_single",
                (update.req_id, update.pos, update.daily_pnl,
                 update.unrealized_pnl, update.realized_pnl, update.value),
                None,
            )?;
        }

        // Account summary dispatch (via ClientCore)
        {
            let acct_name = self.account();
            if let Some(batch) = self.core.prepare_account_summary(shared, acct_name.as_str()) {
                let tags_orig = self.core.account_summary_req.lock().unwrap().clone();
                let tags_list = tags_orig.map(|(_, t)| t).unwrap_or_default();
                if tags_list.is_empty() || tags_list.iter().any(|t| t == "AccountType") {
                    self.wrapper.call_method(
                        py, "account_summary",
                        (batch.req_id, acct_name.as_str(), "AccountType", "INDIVIDUAL", ""),
                        None,
                    )?;
                }
                for entry in &batch.entries {
                    self.wrapper.call_method(
                        py, "account_summary",
                        (batch.req_id, acct_name.as_str(), entry.tag, entry.value.as_str(), entry.currency),
                        None,
                    )?;
                }
                self.wrapper.call_method1(py, "account_summary_end", (batch.req_id,))?;
            }
        }

        Ok(())
    }
}
