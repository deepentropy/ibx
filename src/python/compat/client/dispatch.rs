//! Event dispatch: drains SharedState queues and fires Python wrapper callbacks.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pyo3::prelude::*;

use crate::bridge::{Event, SharedState};
use crate::client_core::order_status_str;
use crate::types::*;

use crate::api::types::{
    Contract as ApiContract, Execution as ApiExecution,
    CommissionReport as ApiCommissionReport,
};
use super::EClient;
use super::super::contract::{Contract, ContractDescription, ContractDetails, BarData, CommissionReport, DepthMktDataDescriptionPy};
use super::super::tick_types::*;
use super::super::super::types::PRICE_SCALE_F;

/// Call a Python wrapper method, catching and logging any exception instead of propagating.
/// This prevents user callback exceptions from killing the dispatch loop.
macro_rules! call_wrapper {
    ($wrapper:expr, $py:expr, $method:expr, $args:expr) => {
        if let Err(e) = $wrapper.call_method($py, $method, $args, None) {
            log::error!("Python callback {}() raised: {}", $method, e);
            e.restore($py);
            unsafe { pyo3::ffi::PyErr_Clear(); }
        }
    };
}

impl EClient {
    /// Single iteration of event dispatch: drain all shared queues and fire Python callbacks.
    pub(crate) fn dispatch_once(&self, py: Python<'_>, shared: &Arc<SharedState>) -> PyResult<()> {
        // Drain engine events — surface disconnects as error callbacks.
        if let Some(rx) = self.event_rx.lock().unwrap().as_ref() {
            while let Ok(event) = rx.try_recv() {
                if matches!(event, Event::Disconnected) {
                    call_wrapper!(self.wrapper, py, "error", (-1i64, 1100i64, "Connectivity between client and server has been lost", ""));
                    self.connected.store(false, Ordering::Release);
                }
            }
        }

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
            let (perm_id, parent_id) = shared.orders.get_order_info(fill.order_id)
                .map(|info| (info.order.perm_id, info.order.parent_id))
                .unwrap_or((0, 0));
            call_wrapper!(self.wrapper, py, "order_status", (fill.order_id as i64, status, fill.qty as f64, fill.remaining as f64,
                 price, perm_id, parent_id, price, 0i64, "", 0.0f64));

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
            // Build api-level contract for shared storage
            let api_contract = self.core.open_orders.lock().unwrap()
                .get(&fill.order_id).map(|o| o.contract.clone())
                .or_else(|| {
                    rich_info.map(|info| info.contract)
                })
                .unwrap_or_default();

            let api_exec = ApiExecution {
                exec_id: exec_id.clone(),
                time: now_str.clone(),
                acct_number: self.account(),
                exchange: exec_exchange.clone(),
                side: side_str.to_string(),
                shares: fill.qty as f64,
                price,
                order_id: fill.order_id as i64,
                cum_qty,
                avg_price,
                ..Default::default()
            };
            let api_commission = ApiCommissionReport {
                exec_id: exec_id.clone(),
                commission,
                currency: "USD".into(),
                realized_pnl: f64::MAX,
                yield_amount: f64::MAX,
                yield_redemption_date: String::new(),
            };

            // Build Python contract for callback
            let exec_contract = Contract {
                con_id: api_contract.con_id,
                symbol: api_contract.symbol.clone(),
                sec_type: api_contract.sec_type.clone(),
                exchange: api_contract.exchange.clone(),
                currency: api_contract.currency.clone(),
                ..Default::default()
            };

            // Store for req_executions replay via shared core
            self.core.push_execution(req_id, api_contract, api_exec, api_commission);

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
            exec_dict.set_item("permId", perm_id)?;
            exec_dict.set_item("clientId", 0i64)?;
            exec_dict.set_item("liquidation", 0i64)?;
            exec_dict.set_item("lastLiquidity", 0i64)?;
            exec_dict.set_item("pendingPriceRevision", false)?;
            call_wrapper!(self.wrapper, py, "exec_details", (req_id, &c_py, exec_dict.as_any()));

            // Update open order tracking
            self.core.update_order_fill(fill.order_id, status, fill.qty as f64, fill.remaining as f64);

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
            call_wrapper!(self.wrapper, py, "commission_report", (&report_py,));
        }

        // Drain order updates -> orderStatus
        let updates = shared.orders.drain_order_updates();
        for update in updates {
            let status = order_status_str(update.status);
            call_wrapper!(self.wrapper, py, "order_status", (update.order_id as i64, status, update.filled_qty as f64,
                 update.remaining_qty as f64, 0.0f64, update.perm_id, update.parent_id, 0.0f64, 0i64, "", 0.0f64));

            // Track open orders
            self.core.update_order_status(update.order_id, status, update.filled_qty as f64, update.remaining_qty as f64);
        }

        // Drain cancel rejects -> error
        let rejects = shared.orders.drain_cancel_rejects();
        for reject in rejects {
            let code = if reject.reject_type == 1 { 202i64 } else { 10147i64 };
            let msg = format!("Order {} cancel/modify rejected (reason: {})", reject.order_id, reject.reason_code);
            call_wrapper!(self.wrapper, py, "error", (reject.order_id as i64, code, msg.as_str(), ""));
        }

        // Poll quotes for changes -> tickPrice/tickSize
        // Poll quotes via shared ClientCore (same logic as Rust dispatch)
        let instruments = self.core.snapshot_instruments();
        let mut snapshot_done: Vec<i64> = Vec::new();
        for (iid, req_id) in instruments {
            let result = self.core.poll_instrument_ticks(shared, iid, req_id);

            // Fire market_data_type once per subscription on first tick delivery
            if let Some(mdt) = self.core.check_mdt_needed(req_id, result.delivered) {
                call_wrapper!(self.wrapper, py, "market_data_type", (req_id, mdt));
            }

            let attrib = TickAttrib::default();
            let attrib_obj = Py::new(py, attrib)?.into_any();
            for tick in &result.ticks {
                if tick.is_price {
                    call_wrapper!(self.wrapper, py, "tick_price", (tick.req_id, tick.tick_type, tick.value, &attrib_obj));
                } else {
                    call_wrapper!(self.wrapper, py, "tick_size", (tick.req_id, tick.tick_type, tick.value));
                }
            }
            if let Some(ts) = &result.timestamp {
                let ts_secs = ts.timestamp_ns / 1_000_000_000;
                call_wrapper!(self.wrapper, py, "tick_string", (ts.req_id, TICK_LAST_TIMESTAMP, ts_secs.to_string().as_str()));
            }
            if self.core.check_snapshot_done(req_id, result.delivered) {
                call_wrapper!(self.wrapper, py, "tick_snapshot_end", (req_id,));
                snapshot_done.push(req_id);
            }
        }
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
            call_wrapper!(self.wrapper, py, "tick_by_tick_all_last", (req_id, 1i32, trade.timestamp as i64, price, size,
                 &attrib_obj, trade.exchange.as_str(), trade.conditions.as_str()));
        }

        // Drain TBT quotes -> tickByTickBidAsk
        let tbt_quotes = shared.market.drain_tbt_quotes();
        for quote in tbt_quotes {
            let req_id = self.core.instrument_to_req.lock().unwrap()
                .get(&quote.instrument).copied().unwrap_or(-1);
            let attrib = super::super::tick_types::TickAttribBidAsk::default();
            let attrib_obj = Py::new(py, attrib)?.into_any();
            call_wrapper!(self.wrapper, py, "tick_by_tick_bid_ask", (req_id, quote.timestamp as i64,
                 quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                 quote.bid_size as f64, quote.ask_size as f64, &attrib_obj));
        }

        // Drain depth updates -> updateMktDepth / updateMktDepthL2
        let depth_updates = shared.market.drain_depth_updates();
        for du in depth_updates {
            if du.market_maker.is_empty() {
                call_wrapper!(self.wrapper, py, "update_mkt_depth", (du.req_id as i64, du.position, du.operation, du.side, du.price, du.size));
            } else {
                call_wrapper!(self.wrapper, py, "update_mkt_depth_l2", (du.req_id as i64, du.position, du.market_maker.as_str(),
                     du.operation, du.side, du.price, du.size, du.is_smart_depth));
            }
        }

        // Drain news -> tickNews
        let news_items = shared.market.drain_tick_news();
        for news in news_items {
            let req_id = self.core.req_id_for_instrument(news.instrument);
            call_wrapper!(self.wrapper, py, "tick_news", (req_id, news.timestamp as i64, news.provider_code.as_str(),
                 news.article_id.as_str(), news.headline.as_str(), ""));
        }

        // Drain news bulletins -> updateNewsBulletin
        if self.core.bulletin_subscribed.load(Ordering::Acquire) {
            let bulletins = shared.market.drain_news_bulletins();
            for b in bulletins {
                call_wrapper!(self.wrapper, py, "update_news_bulletin", (b.msg_id as i64, b.msg_type, b.message.as_str(), b.exchange.as_str()));
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
            call_wrapper!(self.wrapper, py, "order_status", (wi.order_id as i64, "PreSubmitted", 0.0f64, 0.0f64,
                 0.0f64, 0i64, 0i64, 0.0f64, 0i64, msg.as_str(), 0.0f64));
        }

        // Drain historical data -> historicalData + historicalDataEnd / historicalDataUpdate
        let hist_data = shared.reference.drain_historical_data();
        for (req_id, response) in hist_data {
            let is_update = self.core.hist_initial_complete.lock().unwrap().contains(&req_id);
            for bar in &response.bars {
                let bar_obj = BarData::new(
                    bar.time.clone(), bar.open, bar.high, bar.low, bar.close,
                    bar.volume, bar.wap, bar.count as i32,
                );
                let bar_py = Py::new(py, bar_obj)?.into_any();
                if is_update {
                    call_wrapper!(self.wrapper, py, "historical_data_update", (req_id as i64, &bar_py));
                } else {
                    call_wrapper!(self.wrapper, py, "historical_data", (req_id as i64, &bar_py));
                }
            }
            if response.is_complete && !is_update {
                self.core.hist_initial_complete.lock().unwrap().insert(req_id);
                call_wrapper!(self.wrapper, py, "historical_data_end", (req_id as i64, "", ""));
            }
        }

        // Drain head timestamps -> headTimestamp
        let head_ts = shared.reference.drain_head_timestamps();
        for (req_id, response) in head_ts {
            call_wrapper!(self.wrapper, py, "head_timestamp",
                (req_id as i64, response.head_timestamp.as_str()));
        }

        // Drain contract details -> contractDetails + contractDetailsEnd
        let contract_defs = shared.reference.drain_contract_details();
        for (req_id, def) in contract_defs {
            let details = ContractDetails::from_definition(&def);
            let details_py = Py::new(py, details)?.into_any();
            call_wrapper!(self.wrapper, py, "contract_details",
                (req_id as i64, &details_py));
        }
        let contract_ends = shared.reference.drain_contract_details_end();
        for req_id in contract_ends {
            call_wrapper!(self.wrapper, py, "contract_details_end", (req_id as i64,));
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
            call_wrapper!(self.wrapper, py, "symbol_samples", (req_id as i64, list.as_any()));
        }

        // Drain depth exchanges -> mktDepthExchanges
        let depth_exchanges = shared.reference.drain_depth_exchanges();
        if !depth_exchanges.is_empty() {
            let descriptions: Vec<Py<DepthMktDataDescriptionPy>> = depth_exchanges.iter().map(|d| {
                Py::new(py, DepthMktDataDescriptionPy {
                    exchange: d.exchange.clone(),
                    sec_type: d.sec_type.clone(),
                    listing_exch: d.listing_exch.clone(),
                    service_data_type: d.service_data_type.clone(),
                    agg_group: d.agg_group,
                }).unwrap()
            }).collect();
            let list = pyo3::types::PyList::new(py, &descriptions)?;
            call_wrapper!(self.wrapper, py, "mkt_depth_exchanges", (list.as_any(),));
        }

        // Drain scanner params -> scannerParameters
        let scanner_params = shared.reference.drain_scanner_params();
        for xml in scanner_params {
            call_wrapper!(self.wrapper, py, "scanner_parameters", (xml.as_str(),));
        }

        // Drain scanner data -> scannerData + scannerDataEnd
        let scanner_results = shared.reference.drain_scanner_data();
        for (req_id, result) in scanner_results {
            for (rank, &con_id) in result.con_ids.iter().enumerate() {
                let mut cd = ContractDetails::default();
                cd.contract.con_id = con_id as i64;
                let cd_py = Py::new(py, cd)?.into_any();
                call_wrapper!(self.wrapper, py, "scanner_data", (req_id as i64, rank as i32, &cd_py, "", "", "", ""));
            }
            call_wrapper!(self.wrapper, py, "scanner_data_end", (req_id as i64,));
        }

        // Drain historical news -> historicalNews + historicalNewsEnd
        let news_results = shared.reference.drain_historical_news();
        for (req_id, headlines, has_more) in news_results {
            for h in &headlines {
                call_wrapper!(self.wrapper, py, "historical_news", (req_id as i64, h.time.as_str(), h.provider_code.as_str(),
                     h.article_id.as_str(), h.headline.as_str()));
            }
            call_wrapper!(self.wrapper, py, "historical_news_end", (req_id as i64, has_more));
        }

        // Drain news articles -> newsArticle
        let articles = shared.reference.drain_news_articles();
        for (req_id, article_type, text) in articles {
            call_wrapper!(self.wrapper, py, "news_article", (req_id as i64, article_type, text.as_str()));
        }

        // Drain fundamental data -> fundamentalData
        let fundamentals = shared.reference.drain_fundamental_data();
        for (req_id, data) in fundamentals {
            call_wrapper!(self.wrapper, py, "fundamental_data", (req_id as i64, data.as_str()));
        }

        // Drain histogram data -> histogram_data
        let histograms = shared.reference.drain_histogram_data();
        for (req_id, entries) in histograms {
            let tuples: Vec<Bound<'_, pyo3::types::PyTuple>> = entries.iter().map(|e| {
                pyo3::types::PyTuple::new(py, &[e.price.into_pyobject(py).unwrap().into_any(), e.count.into_pyobject(py).unwrap().into_any()]).unwrap()
            }).collect();
            let py_list = pyo3::types::PyList::new(py, tuples)?;
            call_wrapper!(self.wrapper, py, "histogram_data", (req_id as i64, py_list));
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
                    call_wrapper!(self.wrapper, py, "historical_ticks", (req_id as i64, list, done));
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
                    call_wrapper!(self.wrapper, py, "historical_ticks_last", (req_id as i64, list, done));
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
                    call_wrapper!(self.wrapper, py, "historical_ticks_bid_ask", (req_id as i64, list, done));
                }
            }
        }

        // Drain real-time bars -> real_time_bar or historical_data_update (keepUpToDate)
        let rtbars = shared.market.drain_real_time_bars();
        for (req_id, bar) in rtbars {
            if self.core.hist_initial_complete.lock().unwrap().contains(&req_id) {
                // keepUpToDate bar → dispatch as historical_data_update
                let bar_obj = BarData::new(
                    format!("{}", bar.timestamp), bar.open, bar.high, bar.low, bar.close,
                    bar.volume as i64, bar.wap, bar.count,
                );
                let bar_py = Py::new(py, bar_obj)?.into_any();
                call_wrapper!(self.wrapper, py, "historical_data_update", (req_id as i64, &bar_py));
            } else {
                call_wrapper!(self.wrapper, py, "real_time_bar", (
                    req_id as i64,
                    bar.timestamp as i64,
                    bar.open, bar.high, bar.low, bar.close,
                    bar.volume, bar.wap, bar.count,
                ));
            }
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
            call_wrapper!(self.wrapper, py, "historical_schedule", (
                req_id as i64,
                resp.start_date_time.as_str(),
                resp.end_date_time.as_str(),
                resp.timezone.as_str(),
                py_sessions,
            ));
        }

        // Account updates (via ClientCore)
        if let Some(batch) = self.core.prepare_account_updates(shared) {
            let account_name = self.account();
            for field in &batch.fields {
                call_wrapper!(self.wrapper, py, "update_account_value", (field.key, field.value.as_str(), field.currency, account_name.as_str()));
            }

            // Portfolio updates (position entries)
            let portfolio = self.core.prepare_portfolio_updates(shared);
            for entry in &portfolio {
                let contract = self.core.get_contract(entry.con_id, shared);
                let c = contract.map(|ac| {
                    let mut c = crate::python::compat::contract::Contract::default();
                    c.con_id = ac.con_id;
                    c.symbol = ac.symbol;
                    c.sec_type = ac.sec_type;
                    c.exchange = ac.exchange;
                    c.currency = ac.currency;
                    c
                }).unwrap_or_else(|| {
                    let mut c = crate::python::compat::contract::Contract::default();
                    c.con_id = entry.con_id;
                    c
                });
                let c_py = pyo3::Py::new(py, c).unwrap().into_any();
                call_wrapper!(self.wrapper, py, "update_portfolio",
                    (&c_py, entry.position, 0.0_f64, 0.0_f64, entry.avg_cost, 0.0_f64, 0.0_f64, account_name.as_str()));
            }

            if batch.delivered {
                call_wrapper!(self.wrapper, py, "update_account_time", ("",));
                call_wrapper!(self.wrapper, py, "account_download_end", (account_name.as_str(),));
            }
        }

        // P&L dispatch (via ClientCore)
        if let Some(update) = self.core.poll_pnl(shared) {
            call_wrapper!(self.wrapper, py, "pnl", (update.req_id, update.daily_pnl, update.unrealized_pnl, update.realized_pnl));
        }

        // Per-position P&L dispatch (via ClientCore)
        for update in self.core.poll_pnl_single(shared) {
            call_wrapper!(self.wrapper, py, "pnl_single", (update.req_id, update.pos, update.daily_pnl,
                 update.unrealized_pnl, update.realized_pnl, update.value));
        }

        // Account summary dispatch (via ClientCore)
        {
            let acct_name = self.account();
            if let Some(batch) = self.core.prepare_account_summary(shared, acct_name.as_str()) {
                let tags_orig = self.core.account_summary_req.lock().unwrap().clone();
                let tags_list = tags_orig.map(|(_, t)| t).unwrap_or_default();
                if tags_list.is_empty() || tags_list.iter().any(|t| t == "AccountType") {
                    call_wrapper!(self.wrapper, py, "account_summary", (batch.req_id, acct_name.as_str(), "AccountType", "INDIVIDUAL", ""));
                }
                for entry in &batch.entries {
                    call_wrapper!(self.wrapper, py, "account_summary", (batch.req_id, acct_name.as_str(), entry.tag, entry.value.as_str(), entry.currency));
                }
                call_wrapper!(self.wrapper, py, "account_summary_end", (batch.req_id,));
            }
        }

        Ok(())
    }
}
