//! Event dispatch: drains SharedState queues and fires Wrapper callbacks.

use crate::api::types::{
    BarData, CommissionReport, ContractDetails, ContractDescription, Execution,
    TickAttribLast, TickAttribBidAsk, PRICE_SCALE_F,
};
use crate::api::wrapper::Wrapper;
use crate::client_core::order_status_str;
use crate::types::*;

use super::{Contract, EClient};

impl EClient {
    // ── Message Processing ──

    /// Drain all SharedState queues and dispatch to the Wrapper.
    /// Call this in a loop — it is the Rust equivalent of C++ `EReader::processMsgs()`.
    pub fn process_msgs(&self, wrapper: &mut impl Wrapper) {
        self.dispatch_orders(wrapper);
        self.dispatch_quotes(wrapper);
        self.dispatch_data(wrapper);
    }

    // ── Order / Fill Dispatch ──

    fn dispatch_orders(&self, wrapper: &mut impl Wrapper) {
        // Fills → order_status + exec_details + commission_report
        for fill in self.shared.orders.drain_fills() {
            let price_f = fill.price as f64 / PRICE_SCALE_F;
            let commission_f = fill.commission as f64 / PRICE_SCALE_F;
            let status = if fill.remaining == 0 { "Filled" } else { "PartiallyFilled" };
            wrapper.order_status(
                fill.order_id as i64, status, fill.qty as f64, fill.remaining as f64,
                price_f, 0, 0, price_f, 0, "", 0.0,
            );

            let side_str = match fill.side {
                Side::Buy => "BOT",
                Side::Sell => "SLD",
                Side::ShortSell => "SLD",
            };
            let (c, exec) = if let Some(info) = self.shared.orders.get_order_info(fill.order_id) {
                let mut ex = info.last_exec;
                ex.side = side_str.into();
                ex.shares = fill.qty as f64;
                ex.price = price_f;
                ex.order_id = fill.order_id as i64;
                let contract = if info.contract.con_id != 0 {
                    self.core.get_contract(info.contract.con_id, &self.shared).unwrap_or(info.contract)
                } else {
                    info.contract
                };
                (contract, ex)
            } else {
                (Contract::default(), Execution {
                    side: side_str.into(),
                    shares: fill.qty as f64,
                    price: price_f,
                    order_id: fill.order_id as i64,
                    ..Default::default()
                })
            };
            let req_id = self.core.req_id_for_instrument(fill.instrument);
            wrapper.exec_details(req_id, &c, &exec);

            let report = CommissionReport {
                exec_id: exec.exec_id.clone(),
                commission: commission_f,
                currency: "USD".into(),
                realized_pnl: f64::MAX,
                yield_amount: f64::MAX,
                yield_redemption_date: String::new(),
            };
            wrapper.commission_report(&report);

            // Store for req_executions replay
            self.core.push_execution(req_id, c, exec, report);

            // Update open order tracking
            self.core.update_order_fill(fill.order_id, status, fill.qty as f64, fill.remaining as f64);
        }

        // Order updates → order_status
        for update in self.shared.orders.drain_order_updates() {
            let status = order_status_str(update.status);
            wrapper.order_status(
                update.order_id as i64, status, update.filled_qty as f64,
                update.remaining_qty as f64, 0.0, 0, 0, 0.0, 0, "", 0.0,
            );
            self.core.update_order_status(update.order_id, status, update.filled_qty as f64, update.remaining_qty as f64);
        }

        // Cancel rejects → error
        for reject in self.shared.orders.drain_cancel_rejects() {
            let code = if reject.reject_type == 1 { 202 } else { 10147 };
            let msg = format!("Order {} cancel/modify rejected (reason: {})", reject.order_id, reject.reason_code);
            wrapper.error(reject.order_id as i64, code, &msg, "");
        }

        // What-if → order_status (with margin info in why_held)
        for wi in self.shared.orders.drain_what_if_responses() {
            let msg = format!(
                "WhatIf: initMargin={:.2}, maintMargin={:.2}, commission={:.2}",
                wi.init_margin_after as f64 / PRICE_SCALE_F,
                wi.maint_margin_after as f64 / PRICE_SCALE_F,
                wi.commission as f64 / PRICE_SCALE_F,
            );
            wrapper.order_status(
                wi.order_id as i64, "PreSubmitted", 0.0, 0.0, 0.0, 0, 0, 0.0, 0, &msg, 0.0,
            );
        }
    }

    // ── Quote Dispatch ──

    fn dispatch_quotes(&self, wrapper: &mut impl Wrapper) {
        // Quote polling → tick_price / tick_size (via ClientCore)
        let instruments = self.core.snapshot_instruments();
        let attrib = crate::api::types::TickAttrib::default();
        let mut snapshot_done: Vec<i64> = Vec::new();
        for (iid, req_id) in instruments {
            let result = self.core.poll_instrument_ticks(&self.shared, iid, req_id);
            // Fire market_data_type once per subscription on first tick delivery
            if let Some(mdt) = self.core.check_mdt_needed(req_id, result.delivered) {
                wrapper.market_data_type(req_id, mdt);
            }
            for tick in &result.ticks {
                if tick.is_price {
                    wrapper.tick_price(tick.req_id, tick.tick_type, tick.value, &attrib);
                } else {
                    wrapper.tick_size(tick.req_id, tick.tick_type, tick.value);
                }
            }
            if let Some(ts) = &result.timestamp {
                let ts_secs = ts.timestamp_ns / 1_000_000_000;
                wrapper.tick_string(ts.req_id, 45, &ts_secs.to_string());
            }
            if self.core.check_snapshot_done(req_id, result.delivered) {
                wrapper.tick_snapshot_end(req_id);
                snapshot_done.push(req_id);
            }
        }
        for req_id in snapshot_done {
            let _ = self.cancel_mkt_data(req_id);
        }

        // TBT trades → tick_by_tick_all_last
        for trade in self.shared.market.drain_tbt_trades() {
            let req_id = self.core.req_id_for_instrument(trade.instrument);
            let attrib_last = TickAttribLast::default();
            wrapper.tick_by_tick_all_last(
                req_id, 1, trade.timestamp as i64,
                trade.price as f64 / PRICE_SCALE_F, trade.size as f64,
                &attrib_last, &trade.exchange, &trade.conditions,
            );
        }

        // TBT quotes → tick_by_tick_bid_ask
        for quote in self.shared.market.drain_tbt_quotes() {
            let req_id = self.core.req_id_for_instrument(quote.instrument);
            let attrib_ba = TickAttribBidAsk::default();
            wrapper.tick_by_tick_bid_ask(
                req_id, quote.timestamp as i64,
                quote.bid as f64 / PRICE_SCALE_F, quote.ask as f64 / PRICE_SCALE_F,
                quote.bid_size as f64, quote.ask_size as f64, &attrib_ba,
            );
        }
    }

    // ── Historical / News / Account Dispatch ──

    fn dispatch_data(&self, wrapper: &mut impl Wrapper) {
        // News → tick_news
        for news in self.shared.market.drain_tick_news() {
            let req_id = self.core.req_id_for_instrument(news.instrument);
            wrapper.tick_news(
                req_id, news.timestamp as i64,
                &news.provider_code, &news.article_id, &news.headline, "",
            );
        }

        // News bulletins → update_news_bulletin (only when subscribed)
        if self.core.bulletins_subscribed() {
            for b in self.shared.market.drain_news_bulletins() {
                wrapper.update_news_bulletin(b.msg_id as i64, b.msg_type, &b.message, &b.exchange);
            }
        }

        // Historical data → historical_data + historical_data_end
        for (req_id, response) in self.shared.reference.drain_historical_data() {
            for bar in &response.bars {
                let bd = BarData {
                    date: bar.time.clone(),
                    open: bar.open,
                    high: bar.high,
                    low: bar.low,
                    close: bar.close,
                    volume: bar.volume,
                    wap: bar.wap,
                    bar_count: bar.count as i32,
                };
                wrapper.historical_data(req_id as i64, &bd);
            }
            if response.is_complete {
                wrapper.historical_data_end(req_id as i64, "", "");
            }
        }

        // Head timestamps → head_timestamp
        for (req_id, response) in self.shared.reference.drain_head_timestamps() {
            wrapper.head_timestamp(req_id as i64, &response.head_timestamp);
        }

        // Contract details → contract_details + contract_details_end
        for (req_id, def) in self.shared.reference.drain_contract_details() {
            let details = ContractDetails::from_definition(&def);
            wrapper.contract_details(req_id as i64, &details);
        }
        for req_id in self.shared.reference.drain_contract_details_end() {
            wrapper.contract_details_end(req_id as i64);
        }

        // Matching symbols → symbol_samples
        for (req_id, matches) in self.shared.reference.drain_matching_symbols() {
            let descriptions: Vec<ContractDescription> = matches.iter().map(|m| {
                ContractDescription {
                    con_id: m.con_id as i64,
                    symbol: m.symbol.clone(),
                    sec_type: m.sec_type.to_fix().to_string(),
                    currency: m.currency.clone(),
                    primary_exchange: m.primary_exchange.clone(),
                    derivative_sec_types: m.derivative_types.clone(),
                }
            }).collect();
            wrapper.symbol_samples(req_id as i64, &descriptions);
        }

        // Scanner params
        for xml in self.shared.reference.drain_scanner_params() {
            wrapper.scanner_parameters(&xml);
        }

        // Scanner data
        for (req_id, result) in self.shared.reference.drain_scanner_data() {
            for (rank, con_id) in result.con_ids.iter().enumerate() {
                let details = ContractDetails {
                    contract: Contract {
                        con_id: *con_id as i64,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                wrapper.scanner_data(req_id as i64, rank as i32, &details, "", "", "", "");
            }
            wrapper.scanner_data_end(req_id as i64);
        }

        // Historical news
        for (req_id, headlines, has_more) in self.shared.reference.drain_historical_news() {
            for h in &headlines {
                wrapper.historical_news(req_id as i64, &h.time, &h.provider_code, &h.article_id, &h.headline);
            }
            wrapper.historical_news_end(req_id as i64, has_more);
        }

        // News articles
        for (req_id, article_type, text) in self.shared.reference.drain_news_articles() {
            wrapper.news_article(req_id as i64, article_type, &text);
        }

        // Fundamental data
        for (req_id, data) in self.shared.reference.drain_fundamental_data() {
            wrapper.fundamental_data(req_id as i64, &data);
        }

        // Histogram data
        for (req_id, entries) in self.shared.reference.drain_histogram_data() {
            let items: Vec<(f64, i64)> = entries.iter().map(|e| (e.price, e.count)).collect();
            wrapper.histogram_data(req_id as i64, &items);
        }

        // Historical ticks
        for (req_id, data, _query_id, done) in self.shared.reference.drain_historical_ticks() {
            wrapper.historical_ticks(req_id as i64, &data, done);
        }

        // Real-time bars
        for (req_id, bar) in self.shared.market.drain_real_time_bars() {
            wrapper.real_time_bar(
                req_id as i64, bar.timestamp as i64,
                bar.open, bar.high, bar.low, bar.close,
                bar.volume, bar.wap, bar.count,
            );
        }

        // Historical schedules
        for (req_id, schedule) in self.shared.reference.drain_historical_schedules() {
            let sessions: Vec<(String, String, String)> = schedule.sessions.iter()
                .map(|s| (s.ref_date.clone(), s.open_time.clone(), s.close_time.clone()))
                .collect();
            wrapper.historical_schedule(
                req_id as i64, &schedule.start_date_time, &schedule.end_date_time,
                &schedule.timezone, &sessions,
            );
        }

        // PnL → pnl callback (change-detected via ClientCore)
        if let Some(update) = self.core.poll_pnl(&self.shared) {
            wrapper.pnl(update.req_id, update.daily_pnl, update.unrealized_pnl, update.realized_pnl);
        }

        // PnL single → pnl_single callback (via ClientCore)
        for update in self.core.poll_pnl_single(&self.shared) {
            wrapper.pnl_single(update.req_id, update.pos, update.daily_pnl, update.unrealized_pnl, update.realized_pnl, update.value);
        }

        // Account updates → update_account_value + account_download_end (via ClientCore)
        if let Some(batch) = self.core.prepare_account_updates(&self.shared) {
            for field in &batch.fields {
                wrapper.update_account_value(field.key, &field.value, field.currency, &self.account_id);
            }
            if batch.delivered {
                wrapper.update_account_time("");
                wrapper.account_download_end(&self.account_id);
            }
        }

        // Account summary → account_summary + account_summary_end (one-shot via ClientCore)
        if let Some(batch) = self.core.prepare_account_summary(&self.shared, &self.account_id) {
            for entry in &batch.entries {
                wrapper.account_summary(batch.req_id, &self.account_id, entry.tag, &entry.value, entry.currency);
            }
            wrapper.account_summary_end(batch.req_id);
        }
    }
}
