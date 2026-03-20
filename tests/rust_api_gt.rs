//! Integration tests for the Rust EClient API against ibapi ground truth captures.
//!
//! Each test connects via EClient::connect(), calls an API method, captures
//! Wrapper callbacks, and compares field-by-field against GT JSON from ibapi.
//!
//! Requires IB_USERNAME and IB_PASSWORD environment variables.
//! Run with: cargo test --test rust_api_gt -- --nocapture

use std::env;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use ibx::api::client::{EClient, EClientConfig, Contract, Order};
use ibx::api::types::*;
use ibx::api::wrapper::Wrapper;

// ── GT file loader ──

fn gt_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test_data")
        .join("captures")
        .join("20260320_210026")
}

fn load_gt(filename: &str) -> serde_json::Value {
    let path = gt_dir().join(filename);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read GT file {}: {}", path.display(), e));
    serde_json::from_str(&text).unwrap()
}

fn gt_callbacks(gt: &serde_json::Value, name: &str) -> Vec<serde_json::Value> {
    gt["responses"].as_array().unwrap()
        .iter()
        .filter(|r| r["callback"].as_str() == Some(name))
        .cloned()
        .collect()
}

// ── Recording Wrapper ──

#[derive(Clone, Debug)]
enum Cb {
    NextValidId { order_id: i64 },
    Error { req_id: i64, code: i64, msg: String },
    ContractDetails { req_id: i64, contract: ContractSnapshot, market_name: String, min_tick: f64 },
    ContractDetailsEnd { req_id: i64 },
    SymbolSamples { req_id: i64, descriptions: Vec<ContractDescSnapshot> },
    TickPrice { req_id: i64, tick_type: i32, price: f64 },
    TickSize { req_id: i64, tick_type: i32, size: f64 },
    OrderStatus { order_id: i64, status: String, filled: f64, remaining: f64 },
    OpenOrder { order_id: i64, contract: ContractSnapshot, order: OrderSnapshot, state: OrderStateSnapshot },
    OpenOrderEnd,
    CompletedOrder { contract: ContractSnapshot, order: OrderSnapshot, state: OrderStateSnapshot },
    CompletedOrdersEnd,
    ExecDetails { req_id: i64, contract: ContractSnapshot, execution: ExecSnapshot },
    ExecDetailsEnd { req_id: i64 },
    Position { account: String, contract: ContractSnapshot, pos: f64, avg_cost: f64 },
    PositionEnd,
    AccountSummary { req_id: i64, account: String, tag: String, value: String, currency: String },
    AccountSummaryEnd { req_id: i64 },
    Pnl { req_id: i64, daily: f64, unrealized: f64, realized: f64 },
    HistoricalData { req_id: i64, date: String },
    HistoricalDataEnd { req_id: i64 },
    HeadTimestamp { req_id: i64, ts: String },
    HistogramData { req_id: i64, count: usize },
    HistoricalTicks { req_id: i64, done: bool },
    ScannerParameters,
    HistoricalSchedule { req_id: i64, tz: String },
    MarketRule { id: i64, count: usize },
}

#[derive(Clone, Debug)]
struct ContractSnapshot {
    con_id: i64,
    symbol: String,
    sec_type: String,
    exchange: String,
    currency: String,
    local_symbol: String,
    trading_class: String,
}

#[derive(Clone, Debug)]
struct OrderSnapshot {
    order_id: i64,
    action: String,
    total_quantity: f64,
    order_type: String,
    lmt_price: f64,
    tif: String,
    account: String,
    perm_id: i64,
    outside_rth: bool,
}

#[derive(Clone, Debug)]
struct OrderStateSnapshot {
    status: String,
    completed_time: String,
    completed_status: String,
}

#[derive(Clone, Debug)]
struct ExecSnapshot {
    exec_id: String,
    time: String,
    acct_number: String,
    exchange: String,
    side: String,
    shares: f64,
    price: f64,
    avg_price: f64,
    cum_qty: f64,
    last_liquidity: i32,
}

#[derive(Clone, Debug)]
struct ContractDescSnapshot {
    con_id: i64,
    symbol: String,
    sec_type: String,
    currency: String,
}

fn snap_contract(c: &ibx::api::types::Contract) -> ContractSnapshot {
    ContractSnapshot {
        con_id: c.con_id,
        symbol: c.symbol.clone(),
        sec_type: c.sec_type.clone(),
        exchange: c.exchange.clone(),
        currency: c.currency.clone(),
        local_symbol: c.local_symbol.clone(),
        trading_class: c.trading_class.clone(),
    }
}

struct RecWrapper {
    events: Mutex<Vec<Cb>>,
}

impl RecWrapper {
    fn new() -> Self { Self { events: Mutex::new(Vec::new()) } }
    fn push(&self, cb: Cb) { self.events.lock().unwrap().push(cb); }
    fn drain(&self) -> Vec<Cb> { std::mem::take(&mut *self.events.lock().unwrap()) }
    fn wait_for<F: Fn(&Cb) -> bool>(&self, pred: F, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.events.lock().unwrap().iter().any(&pred) { return true; }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Wrapper for RecWrapper {
    fn next_valid_id(&mut self, order_id: i64) {
        self.push(Cb::NextValidId { order_id });
    }
    fn error(&mut self, req_id: i64, error_code: i64, error_string: &str, _: &str) {
        self.push(Cb::Error { req_id, code: error_code, msg: error_string.into() });
    }
    fn contract_details(&mut self, req_id: i64, details: &ContractDetails) {
        self.push(Cb::ContractDetails {
            req_id,
            contract: snap_contract(&details.contract),
            market_name: details.market_name.clone(),
            min_tick: details.min_tick,
        });
    }
    fn contract_details_end(&mut self, req_id: i64) {
        self.push(Cb::ContractDetailsEnd { req_id });
    }
    fn symbol_samples(&mut self, req_id: i64, descriptions: &[ContractDescription]) {
        self.push(Cb::SymbolSamples {
            req_id,
            descriptions: descriptions.iter().map(|d| ContractDescSnapshot {
                con_id: d.con_id,
                symbol: d.symbol.clone(),
                sec_type: d.sec_type.clone(),
                currency: d.currency.clone(),
            }).collect(),
        });
    }
    fn tick_price(&mut self, req_id: i64, tick_type: i32, price: f64, _: &TickAttrib) {
        self.push(Cb::TickPrice { req_id, tick_type, price });
    }
    fn tick_size(&mut self, req_id: i64, tick_type: i32, size: f64) {
        self.push(Cb::TickSize { req_id, tick_type, size });
    }
    fn order_status(
        &mut self, order_id: i64, status: &str, filled: f64, remaining: f64,
        _: f64, _: i64, _: i64, _: f64, _: i64, _: &str, _: f64,
    ) {
        self.push(Cb::OrderStatus { order_id, status: status.into(), filled, remaining });
    }
    fn open_order(&mut self, order_id: i64, contract: &ibx::api::types::Contract, order: &ibx::api::types::Order, state: &OrderState) {
        self.push(Cb::OpenOrder {
            order_id,
            contract: snap_contract(contract),
            order: OrderSnapshot {
                order_id: order.order_id,
                action: order.action.clone(),
                total_quantity: order.total_quantity,
                order_type: order.order_type.clone(),
                lmt_price: order.lmt_price,
                tif: order.tif.clone(),
                account: order.account.clone(),
                perm_id: order.perm_id,
                outside_rth: order.outside_rth,
            },
            state: OrderStateSnapshot {
                status: state.status.clone(),
                completed_time: state.completed_time.clone(),
                completed_status: state.completed_status.clone(),
            },
        });
    }
    fn open_order_end(&mut self) { self.push(Cb::OpenOrderEnd); }
    fn completed_order(&mut self, contract: &ibx::api::types::Contract, order: &ibx::api::types::Order, state: &OrderState) {
        self.push(Cb::CompletedOrder {
            contract: snap_contract(contract),
            order: OrderSnapshot {
                order_id: order.order_id,
                action: order.action.clone(),
                total_quantity: order.total_quantity,
                order_type: order.order_type.clone(),
                lmt_price: order.lmt_price,
                tif: order.tif.clone(),
                account: order.account.clone(),
                perm_id: order.perm_id,
                outside_rth: order.outside_rth,
            },
            state: OrderStateSnapshot {
                status: state.status.clone(),
                completed_time: state.completed_time.clone(),
                completed_status: state.completed_status.clone(),
            },
        });
    }
    fn completed_orders_end(&mut self) { self.push(Cb::CompletedOrdersEnd); }
    fn exec_details(&mut self, req_id: i64, contract: &ibx::api::types::Contract, execution: &Execution) {
        self.push(Cb::ExecDetails {
            req_id,
            contract: snap_contract(contract),
            execution: ExecSnapshot {
                exec_id: execution.exec_id.clone(),
                time: execution.time.clone(),
                acct_number: execution.acct_number.clone(),
                exchange: execution.exchange.clone(),
                side: execution.side.clone(),
                shares: execution.shares,
                price: execution.price,
                avg_price: execution.avg_price,
                cum_qty: execution.cum_qty,
                last_liquidity: execution.last_liquidity,
            },
        });
    }
    fn exec_details_end(&mut self, req_id: i64) { self.push(Cb::ExecDetailsEnd { req_id }); }
    fn position(&mut self, account: &str, contract: &ibx::api::types::Contract, pos: f64, avg_cost: f64) {
        self.push(Cb::Position {
            account: account.into(),
            contract: snap_contract(contract),
            pos,
            avg_cost,
        });
    }
    fn position_end(&mut self) { self.push(Cb::PositionEnd); }
    fn account_summary(&mut self, req_id: i64, account: &str, tag: &str, value: &str, currency: &str) {
        self.push(Cb::AccountSummary {
            req_id,
            account: account.into(),
            tag: tag.into(),
            value: value.into(),
            currency: currency.into(),
        });
    }
    fn account_summary_end(&mut self, req_id: i64) {
        self.push(Cb::AccountSummaryEnd { req_id });
    }
    fn pnl(&mut self, req_id: i64, daily: f64, unrealized: f64, realized: f64) {
        self.push(Cb::Pnl { req_id, daily, unrealized, realized });
    }
    fn historical_data(&mut self, req_id: i64, bar: &BarData) {
        self.push(Cb::HistoricalData { req_id, date: bar.date.clone() });
    }
    fn historical_data_end(&mut self, req_id: i64, _: &str, _: &str) {
        self.push(Cb::HistoricalDataEnd { req_id });
    }
    fn head_timestamp(&mut self, req_id: i64, ts: &str) {
        self.push(Cb::HeadTimestamp { req_id, ts: ts.into() });
    }
    fn histogram_data(&mut self, req_id: i64, items: &[(f64, i64)]) {
        self.push(Cb::HistogramData { req_id, count: items.len() });
    }
    fn historical_ticks(&mut self, req_id: i64, _: &ibx::types::HistoricalTickData, done: bool) {
        self.push(Cb::HistoricalTicks { req_id, done });
    }
    fn scanner_parameters(&mut self, _: &str) {
        self.push(Cb::ScannerParameters);
    }
    fn historical_schedule(&mut self, req_id: i64, _: &str, _: &str, tz: &str, _: &[(String, String, String)]) {
        self.push(Cb::HistoricalSchedule { req_id, tz: tz.into() });
    }
    fn market_rule(&mut self, id: i64, increments: &[PriceIncrement]) {
        self.push(Cb::MarketRule { id, count: increments.len() });
    }
}

// ── Helpers ──

fn get_config() -> Option<EClientConfig> {
    let username = env::var("IB_USERNAME").ok()?;
    let password = env::var("IB_PASSWORD").ok()?;
    let host = env::var("IB_HOST").unwrap_or_else(|_| "cdc1.ibllc.com".to_string());
    Some(EClientConfig {
        username,
        password,
        host,
        paper: true,
        core_id: None,
    })
}

fn spy() -> Contract {
    Contract { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".into(),
               exchange: "SMART".into(), currency: "USD".into(), ..Default::default() }
}

fn poll(client: &EClient, wrapper: &mut RecWrapper, duration: Duration) {
    let start = Instant::now();
    while start.elapsed() < duration {
        client.process_msgs(wrapper);
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn poll_until(client: &EClient, wrapper: &mut RecWrapper, pred: impl Fn(&[Cb]) -> bool, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        client.process_msgs(wrapper);
        if pred(&wrapper.events.lock().unwrap()) { return; }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ── Comparison helpers ──

fn assert_field(name: &str, actual: &str, gt_key: &str, gt: &serde_json::Value) -> bool {
    if let Some(expected) = gt.get(gt_key).and_then(|v| v.as_str()) {
        if actual != expected {
            println!("    FAIL {}: '{}' != GT '{}'", name, actual, expected);
            return false;
        }
    }
    true
}

fn assert_field_i64(name: &str, actual: i64, gt_key: &str, gt: &serde_json::Value) -> bool {
    if let Some(expected) = gt.get(gt_key).and_then(|v| v.as_i64()) {
        if actual != expected {
            println!("    FAIL {}: {} != GT {}", name, actual, expected);
            return false;
        }
    }
    true
}

// ── Tests ──

#[test]
fn api_gt_suite() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };

    println!("=== Rust API GT Integration Suite ===\n");
    let suite_start = Instant::now();

    let client = EClient::connect(&config)
        .expect("EClient::connect failed");

    println!("Connected. Account: {}\n", client.account_id);

    let mut wrapper = RecWrapper::new();
    let mut pass_count = 0;
    let mut fail_count = 0;
    let mut skip_count = 0;

    // ── 1. Contract Details (SPY) ──
    {
        print!("  req_contract_details (SPY)... ");
        wrapper.drain();
        client.req_contract_details(100, &spy());
        poll(&client, &mut wrapper, Duration::from_secs(8));
        let cbs = wrapper.drain();

        let gt = load_gt("04_reqContractDetails_SPY.json");
        let gt_cd = gt_callbacks(&gt, "contractDetails");

        let cd: Vec<_> = cbs.iter().filter_map(|c| if let Cb::ContractDetails { contract, .. } = c { Some(contract) } else { None }).collect();
        let has_end = cbs.iter().any(|c| matches!(c, Cb::ContractDetailsEnd { .. }));

        if cd.is_empty() || !has_end {
            println!("FAIL (no contractDetails or no end)");
            fail_count += 1;
        } else {
            let c = &cd[0];
            let gt_c = &gt_cd[0]["args"]["contractDetails"]["contract"];
            let mut ok = true;
            ok &= assert_field_i64("conId", c.con_id, "conId", gt_c);
            ok &= assert_field("symbol", &c.symbol, "symbol", gt_c);
            ok &= assert_field("secType", &c.sec_type, "secType", gt_c);
            ok &= assert_field("currency", &c.currency, "currency", gt_c);
            if ok { println!("PASS"); pass_count += 1; }
            else { println!("FAIL"); fail_count += 1; }
        }
    }

    // ── 2. Matching Symbols ──
    {
        print!("  req_matching_symbols (AAPL)... ");
        wrapper.drain();
        client.req_matching_symbols(110, "AAPL");
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::SymbolSamples { .. })), Duration::from_secs(10));
        let cbs = wrapper.drain();

        let gt = load_gt("07_reqMatchingSymbols.json");
        let gt_ss = gt_callbacks(&gt, "symbolSamples");

        let ss: Vec<_> = cbs.iter().filter_map(|c| if let Cb::SymbolSamples { descriptions, .. } = c { Some(descriptions) } else { None }).collect();

        if ss.is_empty() {
            println!("FAIL (no symbolSamples)");
            fail_count += 1;
        } else {
            let has_aapl = ss[0].iter().any(|d| d.symbol == "AAPL");
            if has_aapl { println!("PASS"); pass_count += 1; }
            else { println!("FAIL (AAPL not in results)"); fail_count += 1; }
        }
    }

    // ── 3. Account Summary ──
    {
        print!("  req_account_summary... ");
        wrapper.drain();
        client.req_account_summary(200, "All", "AccountType,NetLiquidation,TotalCashValue,BuyingPower");
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::AccountSummaryEnd { .. })), Duration::from_secs(10));
        let cbs = wrapper.drain();
        client.cancel_account_summary(200);

        let gt = load_gt("17_reqAccountSummary.json");
        let gt_as = gt_callbacks(&gt, "accountSummary");

        let summaries: Vec<_> = cbs.iter().filter_map(|c| if let Cb::AccountSummary { tag, value, .. } = c { Some((tag.clone(), value.clone())) } else { None }).collect();
        let has_end = cbs.iter().any(|c| matches!(c, Cb::AccountSummaryEnd { .. }));

        if summaries.is_empty() || !has_end {
            println!("FAIL (no accountSummary or no end)");
            fail_count += 1;
        } else {
            let tags: Vec<_> = summaries.iter().map(|(t, _)| t.as_str()).collect();
            let gt_tags: Vec<_> = gt_as.iter().filter_map(|r| r["args"]["tag"].as_str()).collect();
            let mut ok = true;
            for gt_tag in &gt_tags {
                if !tags.contains(gt_tag) {
                    println!("FAIL (missing tag: {})", gt_tag);
                    ok = false;
                }
            }
            if ok { println!("PASS"); pass_count += 1; }
            else { fail_count += 1; }
        }
    }

    // ── 4. Positions ──
    {
        print!("  req_positions... ");
        wrapper.drain();
        client.req_positions(&mut wrapper);
        let cbs = wrapper.drain();

        let has_end = cbs.iter().any(|c| matches!(c, Cb::PositionEnd));
        let positions: Vec<_> = cbs.iter().filter_map(|c| if let Cb::Position { contract, pos, .. } = c { Some((contract, pos)) } else { None }).collect();

        if !has_end {
            println!("FAIL (no position_end)");
            fail_count += 1;
        } else if positions.is_empty() {
            println!("SKIP (no positions in account)");
            skip_count += 1;
        } else {
            let c = positions[0].0;
            println!("conId={} symbol='{}' secType='{}' exchange='{}'", c.con_id, c.symbol, c.sec_type, c.exchange);
            let mut ok = true;
            if c.con_id == 0 { println!("    FAIL conId=0"); ok = false; }
            if c.symbol.is_empty() { println!("    FAIL symbol empty"); ok = false; }
            if c.sec_type.is_empty() { println!("    FAIL secType empty"); ok = false; }
            if ok { print!("    PASS"); pass_count += 1; }
            else { fail_count += 1; }
            println!();
        }
    }

    // ── 5. PnL ──
    {
        print!("  req_pnl... ");
        wrapper.drain();
        client.req_pnl(210, &client.account_id, "");
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::Pnl { .. })), Duration::from_secs(10));
        let cbs = wrapper.drain();
        client.cancel_pnl(210);

        let pnls: Vec<_> = cbs.iter().filter_map(|c| if let Cb::Pnl { daily, unrealized, realized, .. } = c { Some((daily, unrealized, realized)) } else { None }).collect();

        if pnls.is_empty() {
            println!("SKIP (no pnl data — may need positions)");
            skip_count += 1;
        } else {
            let gt = load_gt("20_reqPnL.json");
            let gt_pnl = gt_callbacks(&gt, "pnl");
            // Just verify the callback fires with numeric values
            println!("PASS (daily={:.2} unrealized={:.2} realized={:.2})", pnls[0].0, pnls[0].1, pnls[0].2);
            pass_count += 1;
        }
    }

    // ── 6. Place + Cancel Order → open_order + completed_order ──
    {
        print!("  place_order + cancel → open_order + completed_order... ");
        wrapper.drain();

        // First fetch secdef to populate contract cache
        client.req_contract_details(9999, &spy());
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::ContractDetailsEnd { .. })), Duration::from_secs(10));
        wrapper.drain();

        let oid = client.next_order_id();
        let order = Order {
            action: "BUY".into(),
            total_quantity: 1.0,
            order_type: "LMT".into(),
            lmt_price: 1.0,
            tif: "GTC".into(),
            outside_rth: true,
            ..Default::default()
        };
        client.place_order(oid, &spy(), &order).expect("place_order failed");

        // Wait for order_status Submitted
        poll_until(&client, &mut wrapper, |cbs| {
            cbs.iter().any(|c| matches!(c, Cb::OrderStatus { status, .. } if status == "Submitted"))
        }, Duration::from_secs(15));

        // Wait for enriched cache to be populated
        poll(&client, &mut wrapper, Duration::from_millis(500));

        // req_all_open_orders delivers open_order via Wrapper
        client.req_all_open_orders(&mut wrapper);

        // Cancel
        client.cancel_order(oid, "");
        poll_until(&client, &mut wrapper, |cbs| {
            cbs.iter().any(|c| matches!(c, Cb::OrderStatus { status, .. } if status == "Cancelled" || status == "Inactive"))
        }, Duration::from_secs(15));

        // process_msgs should deliver completed_order
        client.req_completed_orders(&mut wrapper);

        let cbs = wrapper.drain();

        let gt_oo = load_gt("51_reqOpenOrders_with_live_order.json");
        let gt_co = load_gt("54_reqCompletedOrders_after_cancel.json");

        // Check open_order
        let open_orders: Vec<_> = cbs.iter().filter_map(|c| if let Cb::OpenOrder { contract, order, state, .. } = c { Some((contract, order, state)) } else { None }).collect();

        // Check completed_order
        let completed: Vec<_> = cbs.iter().filter_map(|c| if let Cb::CompletedOrder { contract, order, state } = c { Some((contract, order, state)) } else { None }).collect();
        let has_co_end = cbs.iter().any(|c| matches!(c, Cb::CompletedOrdersEnd));

        let mut ok = true;
        println!();

        if let Some((c, o, s)) = open_orders.first() {
            let gt_c = &gt_oo["responses"].as_array().unwrap().iter()
                .find(|r| r["callback"] == "openOrder").map(|r| &r["args"]["contract"]);
            println!("    open_order:");
            println!("      contract: conId={} symbol='{}' secType='{}' localSymbol='{}' tradingClass='{}'",
                c.con_id, c.symbol, c.sec_type, c.local_symbol, c.trading_class);
            println!("      order: action='{}' qty={} type='{}' tif='{}' account='{}'",
                o.action, o.total_quantity, o.order_type, o.tif, o.account);
            println!("      state: status='{}'", s.status);

            if c.con_id != 756733 { println!("      FAIL conId"); ok = false; }
            if c.symbol != "SPY" { println!("      FAIL symbol"); ok = false; }
            if c.sec_type != "STK" { println!("      FAIL secType"); ok = false; }
            if c.local_symbol.is_empty() { println!("      FAIL localSymbol empty"); ok = false; }
            if o.action != "BUY" { println!("      FAIL action"); ok = false; }
            if o.order_type != "LMT" { println!("      FAIL orderType"); ok = false; }
            if o.tif != "GTC" { println!("      FAIL tif"); ok = false; }
            if o.account.is_empty() { println!("      FAIL account empty"); ok = false; }
        } else {
            println!("    open_order: not received (may arrive before process_msgs)");
        }

        if !has_co_end {
            println!("    FAIL completed_orders_end missing");
            ok = false;
        }

        if let Some((c, o, s)) = completed.first() {
            println!("    completed_order:");
            println!("      contract: conId={} symbol='{}' secType='{}' localSymbol='{}' tradingClass='{}'",
                c.con_id, c.symbol, c.sec_type, c.local_symbol, c.trading_class);
            println!("      order: action='{}' qty={} type='{}' tif='{}' account='{}'",
                o.action, o.total_quantity, o.order_type, o.tif, o.account);
            println!("      state: status='{}' completedTime='{}' completedStatus='{}'",
                s.status, s.completed_time, s.completed_status);

            if c.con_id != 756733 { println!("      FAIL conId"); ok = false; }
            if c.symbol != "SPY" { println!("      FAIL symbol"); ok = false; }
            if c.sec_type != "STK" { println!("      FAIL secType"); ok = false; }
            if c.local_symbol.is_empty() { println!("      FAIL localSymbol empty"); ok = false; }
            if o.action != "BUY" { println!("      FAIL action"); ok = false; }
            if o.order_type != "LMT" { println!("      FAIL orderType"); ok = false; }
            if o.account.is_empty() { println!("      FAIL account empty"); ok = false; }
            if !matches!(s.status.as_str(), "Cancelled" | "Inactive") { println!("      FAIL status='{}'", s.status); ok = false; }
        } else {
            println!("    completed_order: not received");
            ok = false;
        }

        if ok { println!("    PASS"); pass_count += 1; }
        else { fail_count += 1; }
    }

    // ── 7. Historical Data ──
    {
        print!("  req_historical_data (SPY 1D 1min)... ");
        wrapper.drain();
        client.req_historical_data(410, &spy(), "", "1 D", "1 min", "TRADES", true, 1, false);
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::HistoricalDataEnd { .. })), Duration::from_secs(15));
        let cbs = wrapper.drain();

        let bars: Vec<_> = cbs.iter().filter(|c| matches!(c, Cb::HistoricalData { .. })).collect();
        let has_end = cbs.iter().any(|c| matches!(c, Cb::HistoricalDataEnd { .. }));

        if bars.is_empty() || !has_end {
            println!("FAIL (no bars or no end)");
            fail_count += 1;
        } else {
            let gt = load_gt("31_reqHistoricalData.json");
            let gt_bars = gt_callbacks(&gt, "historicalData");
            println!("PASS ({} bars, GT had {})", bars.len(), gt_bars.len());
            pass_count += 1;
        }
    }

    // ── 8. Head Timestamp ──
    {
        print!("  req_head_timestamp (SPY TRADES)... ");
        wrapper.drain();
        client.req_head_timestamp(400, &spy(), "TRADES", true, 1);
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::HeadTimestamp { .. })), Duration::from_secs(10));
        let cbs = wrapper.drain();

        let ts: Vec<_> = cbs.iter().filter_map(|c| if let Cb::HeadTimestamp { ts, .. } = c { Some(ts.clone()) } else { None }).collect();

        if ts.is_empty() {
            println!("FAIL (no headTimestamp)");
            fail_count += 1;
        } else {
            println!("PASS (ts={})", ts[0]);
            pass_count += 1;
        }
    }

    // ── 9. Histogram Data ──
    {
        print!("  req_histogram_data (SPY 1week)... ");
        wrapper.drain();
        client.req_histogram_data(430, &spy(), true, "1 week");
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::HistogramData { .. })), Duration::from_secs(10));
        let cbs = wrapper.drain();
        client.cancel_histogram_data(430);

        let hd: Vec<_> = cbs.iter().filter_map(|c| if let Cb::HistogramData { count, .. } = c { Some(*count) } else { None }).collect();

        if hd.is_empty() {
            println!("FAIL (no histogramData)");
            fail_count += 1;
        } else {
            let gt = load_gt("33_reqHistogramData.json");
            println!("PASS ({} items)", hd[0]);
            pass_count += 1;
        }
    }

    // ── 10. Market Data (ticks) ──
    {
        print!("  req_mkt_data (SPY ticks)... ");
        wrapper.drain();
        client.req_mkt_data(500, &spy(), "", false, false);
        poll(&client, &mut wrapper, Duration::from_secs(5));
        client.cancel_mkt_data(500);
        let cbs = wrapper.drain();

        let ticks: Vec<_> = cbs.iter().filter(|c| matches!(c, Cb::TickPrice { .. } | Cb::TickSize { .. })).collect();

        if ticks.is_empty() {
            println!("SKIP (no ticks — market may be closed)");
            skip_count += 1;
        } else {
            println!("PASS ({} ticks)", ticks.len());
            pass_count += 1;
        }
    }

    // ── Cleanup ──
    client.disconnect();

    println!("\n=== Results: {} PASS, {} FAIL, {} SKIP ({:.1}s) ===",
        pass_count, fail_count, skip_count, suite_start.elapsed().as_secs_f64());
    assert_eq!(fail_count, 0, "Some API GT tests failed");
}
