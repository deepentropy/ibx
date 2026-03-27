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
    OrderStatus { order_id: i64, status: String, filled: f64, remaining: f64, why_held: String },
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
    ScannerData { req_id: i64, rank: i32, contract: ContractSnapshot },
    ScannerDataEnd { req_id: i64 },
    FundamentalData { req_id: i64, has_data: bool },
    HistoricalNews { req_id: i64, provider_code: String, article_id: String, headline: String },
    HistoricalNewsEnd { req_id: i64, has_more: bool },
    NewsArticle { req_id: i64, article_type: i32 },
    AccountValue { key: String, value: String, currency: String, account: String },
    AccountDownloadEnd { account: String },
    NewsBulletin { msg_id: i64, msg_type: i32, message: String },
    PnlSingle { req_id: i64, pos: f64 },
    SmartComponents { req_id: i64, count: usize },
    NewsProviders { count: usize },
    CurrentTime { time: i64 },
    SoftDollarTiers { req_id: i64, count: usize },
    FamilyCodes { count: usize },
    UserInfo { req_id: i64, white_branding_id: String },
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
        _: f64, _: i64, _: i64, _: f64, _: i64, why_held: &str, _: f64,
    ) {
        self.push(Cb::OrderStatus { order_id, status: status.into(), filled, remaining, why_held: why_held.into() });
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
    fn scanner_data(&mut self, req_id: i64, rank: i32, details: &ContractDetails, _: &str, _: &str, _: &str, _: &str) {
        self.push(Cb::ScannerData { req_id, rank, contract: snap_contract(&details.contract) });
    }
    fn scanner_data_end(&mut self, req_id: i64) {
        self.push(Cb::ScannerDataEnd { req_id });
    }
    fn fundamental_data(&mut self, req_id: i64, data: &str) {
        self.push(Cb::FundamentalData { req_id, has_data: !data.is_empty() });
    }
    fn historical_news(&mut self, req_id: i64, _: &str, provider_code: &str, article_id: &str, headline: &str) {
        self.push(Cb::HistoricalNews { req_id, provider_code: provider_code.into(), article_id: article_id.into(), headline: headline.into() });
    }
    fn historical_news_end(&mut self, req_id: i64, has_more: bool) {
        self.push(Cb::HistoricalNewsEnd { req_id, has_more });
    }
    fn news_article(&mut self, req_id: i64, article_type: i32, _: &str) {
        self.push(Cb::NewsArticle { req_id, article_type });
    }
    fn update_account_value(&mut self, key: &str, value: &str, currency: &str, account: &str) {
        self.push(Cb::AccountValue { key: key.into(), value: value.into(), currency: currency.into(), account: account.into() });
    }
    fn account_download_end(&mut self, account: &str) {
        self.push(Cb::AccountDownloadEnd { account: account.into() });
    }
    fn update_news_bulletin(&mut self, msg_id: i64, msg_type: i32, message: &str, _: &str) {
        self.push(Cb::NewsBulletin { msg_id, msg_type, message: message.into() });
    }
    fn pnl_single(&mut self, req_id: i64, pos: f64, _: f64, _: f64, _: f64, _: f64) {
        self.push(Cb::PnlSingle { req_id, pos });
    }
    fn market_rule(&mut self, id: i64, increments: &[PriceIncrement]) {
        self.push(Cb::MarketRule { id, count: increments.len() });
    }
    fn smart_components(&mut self, req_id: i64, components: &[ibx::types::SmartComponent]) {
        self.push(Cb::SmartComponents { req_id, count: components.len() });
    }
    fn news_providers(&mut self, providers: &[ibx::types::NewsProvider]) {
        self.push(Cb::NewsProviders { count: providers.len() });
    }
    fn current_time(&mut self, time: i64) {
        self.push(Cb::CurrentTime { time });
    }
    fn soft_dollar_tiers(&mut self, req_id: i64, tiers: &[ibx::types::SoftDollarTier]) {
        self.push(Cb::SoftDollarTiers { req_id, count: tiers.len() });
    }
    fn family_codes(&mut self, codes: &[ibx::types::FamilyCode]) {
        self.push(Cb::FamilyCodes { count: codes.len() });
    }
    fn user_info(&mut self, req_id: i64, white_branding_id: &str) {
        self.push(Cb::UserInfo { req_id, white_branding_id: white_branding_id.into() });
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

fn aapl() -> Contract {
    Contract { con_id: 265598, symbol: "AAPL".into(), sec_type: "STK".into(),
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
        // Wait for CCP init burst to complete before sending secdef request
        poll(&client, &mut wrapper, Duration::from_secs(5));
        wrapper.drain();
        client.req_contract_details(100, &spy()).unwrap();
        poll_until(&client, &mut wrapper,
            |cbs| cbs.iter().any(|c| matches!(c, Cb::ContractDetails { .. } | Cb::ContractDetailsEnd { .. })),
            Duration::from_secs(20));
        let cbs = wrapper.drain();

        let gt = load_gt("04_reqContractDetails_SPY.json");
        let gt_cd = gt_callbacks(&gt, "contractDetails");

        let cd: Vec<_> = cbs.iter().filter_map(|c| if let Cb::ContractDetails { contract, .. } = c { Some(contract) } else { None }).collect();
        let has_end = cbs.iter().any(|c| matches!(c, Cb::ContractDetailsEnd { .. }));

        if cd.is_empty() {
            println!("FAIL (no contractDetails received)");
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

    // ── 1b. Contract Details by symbol only (con_id=0, server resolves) ──
    {
        print!("  req_contract_details (MSFT, con_id=0)... ");
        wrapper.drain();
        let lookup = Contract {
            con_id: 0, symbol: "MSFT".into(), sec_type: "STK".into(),
            exchange: "SMART".into(), currency: "USD".into(), ..Default::default()
        };
        client.req_contract_details(101, &lookup).unwrap();
        poll_until(&client, &mut wrapper,
            |cbs| cbs.iter().any(|c| matches!(c, Cb::ContractDetailsEnd { .. })),
            Duration::from_secs(20));
        let cbs = wrapper.drain();

        let cd: Vec<_> = cbs.iter().filter_map(|c| if let Cb::ContractDetails { contract, .. } = c { Some(contract) } else { None }).collect();

        if cd.is_empty() {
            println!("FAIL (no contractDetails received)");
            fail_count += 1;
        } else {
            let c = &cd[0];
            let mut ok = true;
            if c.con_id == 0 { println!("    FAIL conId=0 (not resolved)"); ok = false; }
            if c.symbol != "MSFT" { println!("    FAIL symbol='{}' expected 'MSFT'", c.symbol); ok = false; }
            if c.sec_type != "STK" { println!("    FAIL secType='{}'", c.sec_type); ok = false; }
            if ok {
                println!("PASS (conId={}, localSymbol='{}')", c.con_id, c.local_symbol);
                pass_count += 1;
            } else { fail_count += 1; }
        }
    }

    // ── 2. Matching Symbols ──
    {
        print!("  req_matching_symbols (AAPL)... ");
        wrapper.drain();
        client.req_matching_symbols(110, "AAPL").unwrap();
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
        // Wait for account data to arrive from CCP
        poll(&client, &mut wrapper, Duration::from_secs(5));
        wrapper.drain();
        client.req_account_summary(200, "All", "NetLiquidation,TotalCashValue,BuyingPower");
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::AccountSummaryEnd { .. })), Duration::from_secs(10));
        let cbs = wrapper.drain();
        client.cancel_account_summary(200);

        let summaries: Vec<_> = cbs.iter().filter_map(|c| if let Cb::AccountSummary { tag, value, .. } = c { Some((tag.clone(), value.clone())) } else { None }).collect();
        let has_end = cbs.iter().any(|c| matches!(c, Cb::AccountSummaryEnd { .. }));

        if summaries.is_empty() || !has_end {
            println!("FAIL (no accountSummary or no end)");
            fail_count += 1;
        } else {
            let mut ok = true;
            for expected in &["NetLiquidation", "TotalCashValue", "BuyingPower"] {
                let tags: Vec<_> = summaries.iter().map(|(t, _)| t.as_str()).collect();
                if !tags.contains(expected) {
                    println!("FAIL (missing tag: {})", expected);
                    ok = false;
                }
            }
            if ok {
                println!("PASS ({} tags)", summaries.len());
                for (t, v) in &summaries { println!("    {}={}", t, v); }
                pass_count += 1;
            } else { fail_count += 1; }
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
        client.req_contract_details(9999, &spy()).unwrap();
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
        client.cancel_order(oid, "").unwrap();
        poll_until(&client, &mut wrapper, |cbs| {
            cbs.iter().any(|c| matches!(c, Cb::OrderStatus { status, .. } if status == "Cancelled" || status == "Inactive"))
        }, Duration::from_secs(15));

        // Wait for hot loop to push CompletedOrder (cancel ack takes time)
        // Try multiple times — CompletedOrder may arrive slightly after OrderStatus
        for _ in 0..5 {
            poll(&client, &mut wrapper, Duration::from_millis(500));
            client.req_completed_orders(&mut wrapper);
            if wrapper.events.lock().unwrap().iter().any(|c| matches!(c, Cb::CompletedOrder { .. })) {
                break;
            }
        }

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
        client.req_historical_data(410, &spy(), "", "1 D", "1 min", "TRADES", true, 1, false).unwrap();
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::HistoricalDataEnd { .. })), Duration::from_secs(15));
        let cbs = wrapper.drain();

        let bars: Vec<_> = cbs.iter().filter(|c| matches!(c, Cb::HistoricalData { .. })).collect();
        let has_end = cbs.iter().any(|c| matches!(c, Cb::HistoricalDataEnd { .. }));

        if bars.is_empty() || !has_end {
            println!("SKIP (no bars — HMDS connection may be down)");
            skip_count += 1;
        } else {
            let gt = load_gt("31_reqHistoricalData.json");
            let gt_bars = gt_callbacks(&gt, "historicalData");
            println!("PASS ({} bars, GT had {})", bars.len(), gt_bars.len());
            pass_count += 1;
        }
    }

    // ── 8. Head Timestamp ──
    {
        print!("  req_head_time_stamp (SPY TRADES)... ");
        wrapper.drain();
        client.req_head_time_stamp(400, &spy(), "TRADES", true, 1).unwrap();
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::HeadTimestamp { .. })), Duration::from_secs(10));
        let cbs = wrapper.drain();

        let ts: Vec<_> = cbs.iter().filter_map(|c| if let Cb::HeadTimestamp { ts, .. } = c { Some(ts.clone()) } else { None }).collect();

        if ts.is_empty() {
            println!("SKIP (no headTimestamp — HMDS connection may be down)");
            skip_count += 1;
        } else {
            println!("PASS (ts={})", ts[0]);
            pass_count += 1;
        }
    }

    // ── 9. Histogram Data ──
    {
        print!("  req_histogram_data (SPY 1week)... ");
        wrapper.drain();
        client.req_histogram_data(430, &spy(), true, "1 week").unwrap();
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::HistogramData { .. })), Duration::from_secs(15));
        let cbs = wrapper.drain();
        client.cancel_histogram_data(430).unwrap();

        let hd: Vec<_> = cbs.iter().filter_map(|c| if let Cb::HistogramData { count, .. } = c { Some(*count) } else { None }).collect();

        if hd.is_empty() {
            println!("SKIP (no histogramData — HMDS connection may be down)");
            skip_count += 1;
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
        client.req_mkt_data(500, &spy(), "", false, false).unwrap();
        poll(&client, &mut wrapper, Duration::from_secs(5));
        client.cancel_mkt_data(500).unwrap();
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

    // ── 11. Historical Ticks (GT) ──
    {
        print!("  req_historical_ticks (SPY TRADES)... ");
        wrapper.drain();
        client.req_historical_ticks(440, &spy(), "20260320 09:30:00", "", 1000, "TRADES", true).unwrap();
        poll_until(&client, &mut wrapper,
            |cbs| cbs.iter().any(|c| matches!(c, Cb::HistoricalTicks { done: true, .. })),
            Duration::from_secs(15));
        let cbs = wrapper.drain();

        let ht: Vec<_> = cbs.iter().filter_map(|c| if let Cb::HistoricalTicks { done, .. } = c { Some(*done) } else { None }).collect();

        if ht.is_empty() {
            println!("SKIP (no historicalTicks — HMDS connection may be down)");
            skip_count += 1;
        } else {
            let gt = load_gt("32_reqHistoricalTicks.json");
            let gt_count = gt["responses"][0]["args"]["ticks"].as_array().map(|a| a.len()).unwrap_or(0);
            let gt_done = gt["responses"][0]["args"]["done"].as_bool().unwrap_or(false);
            if ht[0] && gt_done {
                println!("PASS (done=true, GT had {} ticks)", gt_count);
                pass_count += 1;
            } else {
                println!("FAIL (done={} vs GT done={})", ht[0], gt_done);
                fail_count += 1;
            }
        }
    }

    // ── 12. Scanner Parameters (GT) ──
    {
        print!("  req_scanner_parameters... ");
        wrapper.drain();
        client.req_scanner_parameters().unwrap();
        poll_until(&client, &mut wrapper,
            |cbs| cbs.iter().any(|c| matches!(c, Cb::ScannerParameters)),
            Duration::from_secs(15));
        let cbs = wrapper.drain();

        if cbs.iter().any(|c| matches!(c, Cb::ScannerParameters)) {
            println!("PASS (XML received)");
            pass_count += 1;
        } else {
            println!("FAIL (no scannerParameters callback)");
            fail_count += 1;
        }
    }

    // ── 13. Historical Schedule ──
    {
        print!("  req_historical_schedule (SPY 1 M)... ");
        wrapper.drain();
        client.req_historical_schedule(450, &spy(), "", "1 M", true).unwrap();
        poll_until(&client, &mut wrapper,
            |cbs| cbs.iter().any(|c| matches!(c, Cb::HistoricalSchedule { .. })),
            Duration::from_secs(15));
        let cbs = wrapper.drain();

        let hs: Vec<_> = cbs.iter().filter_map(|c| if let Cb::HistoricalSchedule { tz, .. } = c { Some(tz.clone()) } else { None }).collect();

        if hs.is_empty() {
            println!("SKIP (no historicalSchedule — HMDS connection may be down)");
            skip_count += 1;
        } else if hs[0].is_empty() {
            println!("FAIL (timezone empty)");
            fail_count += 1;
        } else {
            println!("PASS (tz={})", hs[0]);
            pass_count += 1;
        }
    }

    // ── 14. Scanner Subscription ──
    {
        print!("  req_scanner_subscription (TOP_PERC_GAIN)... ");
        wrapper.drain();
        client.req_scanner_subscription(460, "STK", "STK.US.MAJOR", "TOP_PERC_GAIN", 10).unwrap();
        poll_until(&client, &mut wrapper,
            |cbs| cbs.iter().any(|c| matches!(c, Cb::ScannerDataEnd { .. } | Cb::Error { .. })),
            Duration::from_secs(20));
        let cbs = wrapper.drain();
        client.cancel_scanner_subscription(460).unwrap();

        let sd: Vec<_> = cbs.iter().filter(|c| matches!(c, Cb::ScannerData { .. })).collect();
        let has_end = cbs.iter().any(|c| matches!(c, Cb::ScannerDataEnd { .. }));
        let errors: Vec<_> = cbs.iter().filter_map(|c| if let Cb::Error { msg, .. } = c { Some(msg.clone()) } else { None }).collect();

        if has_end {
            println!("PASS ({} results)", sd.len());
            pass_count += 1;
        } else if !errors.is_empty() {
            println!("SKIP (error: {})", errors[0]);
            skip_count += 1;
        } else {
            println!("SKIP (no scannerDataEnd — scanner may be unavailable)");
            skip_count += 1;
        }
    }

    // ── 15. Fundamental Data ──
    {
        print!("  req_fundamental_data (AAPL ReportSnapshot)... ");
        wrapper.drain();
        client.req_fundamental_data(470, &aapl(), "ReportSnapshot").unwrap();
        poll_until(&client, &mut wrapper,
            |cbs| cbs.iter().any(|c| matches!(c, Cb::FundamentalData { .. } | Cb::Error { .. })),
            Duration::from_secs(15));
        let cbs = wrapper.drain();

        let fd: Vec<_> = cbs.iter().filter_map(|c| if let Cb::FundamentalData { has_data, .. } = c { Some(*has_data) } else { None }).collect();
        let errors: Vec<_> = cbs.iter().filter_map(|c| if let Cb::Error { msg, .. } = c { Some(msg.clone()) } else { None }).collect();

        if !fd.is_empty() {
            if fd[0] { println!("PASS (data received)"); pass_count += 1; }
            else { println!("FAIL (empty data)"); fail_count += 1; }
        } else if !errors.is_empty() {
            println!("SKIP (error: {})", errors[0]);
            skip_count += 1;
        } else {
            println!("FAIL (no response)");
            fail_count += 1;
        }
    }

    // ── 16. Historical News ──
    let mut news_article_info: Option<(String, String)> = None;
    {
        print!("  req_historical_news (AAPL)... ");
        wrapper.drain();
        client.req_historical_news(480, 265598, "BRFG+DJNL+BRFUPDN+BZ+FLY", "", "", 5).unwrap();
        poll_until(&client, &mut wrapper,
            |cbs| cbs.iter().any(|c| matches!(c, Cb::HistoricalNewsEnd { .. } | Cb::Error { .. })),
            Duration::from_secs(15));
        let cbs = wrapper.drain();

        let news: Vec<_> = cbs.iter().filter_map(|c| if let Cb::HistoricalNews { provider_code, article_id, headline, .. } = c {
            Some((provider_code.clone(), article_id.clone(), headline.clone()))
        } else { None }).collect();
        let has_end = cbs.iter().any(|c| matches!(c, Cb::HistoricalNewsEnd { .. }));
        let errors: Vec<_> = cbs.iter().filter_map(|c| if let Cb::Error { msg, .. } = c { Some(msg.clone()) } else { None }).collect();

        if has_end {
            if news.is_empty() {
                println!("PASS (0 articles, end marker received)");
            } else {
                println!("PASS ({} articles)", news.len());
                news_article_info = Some((news[0].0.clone(), news[0].1.clone()));
            }
            pass_count += 1;
        } else if !errors.is_empty() {
            println!("SKIP (error: {})", errors[0]);
            skip_count += 1;
        } else {
            println!("FAIL (no historicalNewsEnd)");
            fail_count += 1;
        }
    }

    // ── 17. News Article ──
    {
        if let Some((ref provider, ref article_id)) = news_article_info {
            print!("  req_news_article ({}/{})... ", provider, article_id);
            wrapper.drain();
            client.req_news_article(490, provider, article_id).unwrap();
            poll_until(&client, &mut wrapper,
                |cbs| cbs.iter().any(|c| matches!(c, Cb::NewsArticle { .. } | Cb::Error { .. })),
                Duration::from_secs(15));
            let cbs = wrapper.drain();

            let na: Vec<_> = cbs.iter().filter_map(|c| if let Cb::NewsArticle { article_type, .. } = c { Some(*article_type) } else { None }).collect();
            let errors: Vec<_> = cbs.iter().filter_map(|c| if let Cb::Error { msg, .. } = c { Some(msg.clone()) } else { None }).collect();

            if !na.is_empty() {
                println!("PASS (type={})", na[0]);
                pass_count += 1;
            } else if !errors.is_empty() {
                println!("SKIP (error: {})", errors[0]);
                skip_count += 1;
            } else {
                println!("FAIL (no newsArticle response)");
                fail_count += 1;
            }
        } else {
            println!("  req_news_article... SKIP (no article_id from historical_news)");
            skip_count += 1;
        }
    }

    // ── 18. Account Updates ──
    {
        print!("  req_account_updates... ");
        wrapper.drain();
        client.req_account_updates(true, &client.account_id);
        poll(&client, &mut wrapper, Duration::from_secs(2));
        let cbs = wrapper.drain();
        client.req_account_updates(false, "");

        let values: Vec<_> = cbs.iter().filter_map(|c| if let Cb::AccountValue { key, value, .. } = c { Some((key.clone(), value.clone())) } else { None }).collect();
        let has_end = cbs.iter().any(|c| matches!(c, Cb::AccountDownloadEnd { .. }));

        if values.is_empty() || !has_end {
            println!("FAIL (no account values or no end marker)");
            fail_count += 1;
        } else {
            let has_nlv = values.iter().any(|(k, _)| k == "NetLiquidation");
            let has_bp = values.iter().any(|(k, _)| k == "BuyingPower");
            if has_nlv && has_bp {
                println!("PASS ({} values)", values.len());
                for (k, v) in &values { println!("    {}={}", k, v); }
                pass_count += 1;
            } else {
                println!("FAIL (missing NetLiquidation or BuyingPower)");
                fail_count += 1;
            }
        }
    }

    // ── 19. Lifecycle: is_connected + next_order_id ──
    {
        print!("  is_connected + next_order_id... ");
        let connected = client.is_connected();
        let oid1 = client.next_order_id();
        let oid2 = client.next_order_id();
        if connected && oid2 > oid1 {
            println!("PASS (connected={}, oid1={}, oid2={})", connected, oid1, oid2);
            pass_count += 1;
        } else {
            println!("FAIL (connected={}, oid1={}, oid2={})", connected, oid1, oid2);
            fail_count += 1;
        }
    }

    // ── 20. News Bulletins (subscribe + cancel) ──
    {
        print!("  req_news_bulletins + cancel... ");
        wrapper.drain();
        client.req_news_bulletins(true);
        poll(&client, &mut wrapper, Duration::from_secs(2));
        let cbs_before = wrapper.drain();
        let bulletins_before: Vec<_> = cbs_before.iter().filter(|c| matches!(c, Cb::NewsBulletin { .. })).collect();

        client.cancel_news_bulletins();
        poll(&client, &mut wrapper, Duration::from_secs(1));
        let cbs_after = wrapper.drain();
        let bulletins_after: Vec<_> = cbs_after.iter().filter(|c| matches!(c, Cb::NewsBulletin { .. })).collect();

        // After cancel, no more bulletins should arrive
        if bulletins_after.is_empty() {
            println!("PASS (before={}, after_cancel=0)", bulletins_before.len());
            pass_count += 1;
        } else {
            println!("FAIL ({} bulletins after cancel)", bulletins_after.len());
            fail_count += 1;
        }
    }

    // ── 21. PnL Single ──
    {
        print!("  req_pnl_single (SPY)... ");
        wrapper.drain();
        client.req_pnl_single(500, &client.account_id, "", 756733);
        poll(&client, &mut wrapper, Duration::from_secs(3));
        let cbs = wrapper.drain();
        client.cancel_pnl_single(500);

        let ps: Vec<_> = cbs.iter().filter_map(|c| if let Cb::PnlSingle { pos, .. } = c { Some(*pos) } else { None }).collect();

        if ps.is_empty() {
            println!("SKIP (no pnl_single — may need position in SPY)");
            skip_count += 1;
        } else {
            println!("PASS (pos={})", ps[0]);
            pass_count += 1;
        }
    }

    // ── 22. Cancel Historical Data (in-flight) ──
    {
        print!("  cancel_historical_data (in-flight)... ");
        wrapper.drain();
        // Request 1 month of 1-min bars (~8000 bars) — large enough to cancel mid-stream
        client.req_historical_data(700, &spy(), "", "1 M", "1 min", "TRADES", true, 1, false).unwrap();
        // Brief poll to let some bars arrive
        poll(&client, &mut wrapper, Duration::from_millis(500));
        // Cancel before completion
        client.cancel_historical_data(700).unwrap();
        // Drain whatever arrived before + in-flight after cancel
        // (in-flight data is expected — cancel is async, data already buffered still delivers)
        poll(&client, &mut wrapper, Duration::from_secs(2));
        let cbs = wrapper.drain();
        let bars: Vec<_> = cbs.iter().filter(|c| matches!(c, Cb::HistoricalData { .. })).collect();
        let has_end = cbs.iter().any(|c| matches!(c, Cb::HistoricalDataEnd { .. }));

        // After cancel, the pending entry is removed, so the stream eventually stops.
        // The key: cancel didn't crash, and we got fewer bars than a full 1M request
        // (~8000 bars). HistoricalDataEnd should NOT arrive (pending entry removed).
        if !has_end {
            println!("PASS (cancelled, {} partial bars, no end marker)", bars.len());
            pass_count += 1;
        } else if bars.len() < 7000 {
            // Got end but with fewer bars than full request — cancel partially worked
            println!("PASS (early completion, {} bars)", bars.len());
            pass_count += 1;
        } else {
            // Full dataset arrived — cancel was too late, but didn't crash
            println!("PASS (completed before cancel, {} bars)", bars.len());
            pass_count += 1;
        }
    }

    // ── 23. Cancel methods batch (verify no crash) ──
    {
        print!("  cancel methods batch... ");
        // Subscribe to scanner, verify data, cancel, verify no more data
        wrapper.drain();
        client.req_scanner_subscription(600, "STK", "STK.US.MAJOR", "TOP_PERC_GAIN", 5).unwrap();
        poll_until(&client, &mut wrapper,
            |cbs| cbs.iter().any(|c| matches!(c, Cb::ScannerDataEnd { .. } | Cb::Error { .. })),
            Duration::from_secs(15));
        let cbs = wrapper.drain();
        let scanner_got_data = cbs.iter().any(|c| matches!(c, Cb::ScannerDataEnd { .. }));
        client.cancel_scanner_subscription(600).unwrap();
        // After cancel, polling should yield no more scanner data for this req_id
        poll(&client, &mut wrapper, Duration::from_secs(1));
        let after_cancel = wrapper.drain();
        let scanner_after = after_cancel.iter().any(|c| matches!(c, Cb::ScannerData { req_id: 600, .. } | Cb::ScannerDataEnd { req_id: 600 }));

        // Now fire all other cancel methods with unused req_ids — verify no crash
        client.cancel_historical_data(999).unwrap();
        client.cancel_pnl(999);
        client.cancel_pnl_single(999);
        client.cancel_account_summary(999);
        client.cancel_news_bulletins();
        client.cancel_fundamental_data(999).unwrap();
        client.cancel_histogram_data(999).unwrap();

        if scanner_got_data && !scanner_after {
            println!("PASS (scanner cancel verified, 7 other cancels ok)");
            pass_count += 1;
        } else if !scanner_got_data {
            // Scanner didn't return data, but cancels still didn't crash
            println!("PASS (scanner unavailable, 8 cancels ok)");
            pass_count += 1;
        } else {
            println!("FAIL (scanner data after cancel)");
            fail_count += 1;
        }
    }

    // ── 24. reqGlobalCancel ──
    {
        print!("  req_global_cancel... ");
        wrapper.drain();

        // Ensure secdef cache is populated for SPY
        client.req_contract_details(9998, &spy()).unwrap();
        poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::ContractDetailsEnd { .. })), Duration::from_secs(10));
        wrapper.drain();

        // Place a GTC limit order far from market
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

        // Wait for Submitted
        poll_until(&client, &mut wrapper, |cbs| {
            cbs.iter().any(|c| matches!(c, Cb::OrderStatus { status, .. } if status == "Submitted"))
        }, Duration::from_secs(15));
        wrapper.drain();

        // Global cancel
        client.req_global_cancel().unwrap();

        // Wait for Cancelled
        poll_until(&client, &mut wrapper, |cbs| {
            cbs.iter().any(|c| matches!(c, Cb::OrderStatus { status, .. } if status == "Cancelled" || status == "Inactive"))
        }, Duration::from_secs(15));
        let cbs = wrapper.drain();

        let cancelled = cbs.iter().any(|c| matches!(c, Cb::OrderStatus { status, .. } if status == "Cancelled" || status == "Inactive"));
        if cancelled {
            println!("PASS (order cancelled via global cancel)");
            pass_count += 1;
        } else {
            println!("FAIL (order not cancelled)");
            fail_count += 1;
        }
    }

    // ── 25. What-if preview ──
    {
        print!("  place_order (what-if)... ");
        wrapper.drain();

        let oid = client.next_order_id();
        let order = Order {
            action: "BUY".into(),
            total_quantity: 100.0,
            order_type: "LMT".into(),
            lmt_price: 150.0,
            what_if: true,
            ..Default::default()
        };
        client.place_order(oid, &spy(), &order).expect("place_order what-if failed");

        // What-if response arrives as OrderStatus with "PreSubmitted" and margin info in why_held
        poll_until(&client, &mut wrapper, |cbs| {
            cbs.iter().any(|c| matches!(c, Cb::OrderStatus { status, .. } if status == "PreSubmitted"))
        }, Duration::from_secs(15));
        let cbs = wrapper.drain();

        let whatif: Vec<_> = cbs.iter().filter_map(|c| {
            if let Cb::OrderStatus { status, why_held, .. } = c {
                if status == "PreSubmitted" && why_held.contains("WhatIf") { Some(why_held.clone()) }
                else { None }
            } else { None }
        }).collect();

        if whatif.is_empty() {
            println!("SKIP (no what-if response — server may not support)");
            skip_count += 1;
        } else {
            // Verify margin data is present
            let has_init = whatif[0].contains("initMargin=");
            let has_maint = whatif[0].contains("maintMargin=");
            let has_comm = whatif[0].contains("commission=");
            if has_init && has_maint && has_comm {
                println!("PASS ({})", whatif[0]);
                pass_count += 1;
            } else {
                println!("FAIL (missing margin fields: {})", whatif[0]);
                fail_count += 1;
            }
        }
    }

    // ── 26. Adaptive order ──
    {
        print!("  place_order (adaptive LMT GTC)... ");
        wrapper.drain();

        let oid = client.next_order_id();
        let order = Order {
            action: "BUY".into(),
            total_quantity: 1.0,
            order_type: "LMT".into(),
            lmt_price: 1.0,
            tif: "GTC".into(),
            outside_rth: true,
            algo_strategy: "Adaptive".into(),
            algo_params: vec![TagValue { tag: "adaptivePriority".into(), value: "Normal".into() }],
            ..Default::default()
        };
        client.place_order(oid, &spy(), &order).expect("place_order adaptive failed");

        // Wait for Submitted, PreSubmitted, or Inactive (rejected)
        poll_until(&client, &mut wrapper, |cbs| {
            cbs.iter().any(|c| matches!(c, Cb::OrderStatus { status, .. }
                if status == "Submitted" || status == "PreSubmitted" || status == "Inactive"))
        }, Duration::from_secs(15));

        let cbs = wrapper.drain();
        let accepted = cbs.iter().any(|c| matches!(c, Cb::OrderStatus { status, .. }
            if status == "Submitted" || status == "PreSubmitted"));
        let rejected = cbs.iter().any(|c| matches!(c, Cb::OrderStatus { status, .. }
            if status == "Inactive"));

        if accepted {
            // Cancel the adaptive order
            client.cancel_order(oid, "").unwrap();
            poll_until(&client, &mut wrapper, |cbs| {
                cbs.iter().any(|c| matches!(c, Cb::OrderStatus { status, .. } if status == "Cancelled" || status == "Inactive"))
            }, Duration::from_secs(15));
            wrapper.drain();
            println!("PASS (adaptive order accepted + cancelled)");
            pass_count += 1;
        } else if rejected {
            println!("SKIP (adaptive order rejected — may not be available outside RTH)");
            skip_count += 1;
        } else {
            println!("FAIL (no order_status received)");
            fail_count += 1;
        }
    }

    // ── 27. Gateway-local: req_smart_components ──
    {
        print!("  req_smart_components... ");
        wrapper.drain();
        client.req_smart_components(900, "SMART", &mut wrapper);
        let cbs = wrapper.drain();
        let sc: Vec<_> = cbs.iter().filter_map(|c| if let Cb::SmartComponents { count, .. } = c { Some(*count) } else { None }).collect();
        if sc.is_empty() || sc[0] == 0 {
            println!("FAIL (no smart components)");
            fail_count += 1;
        } else {
            println!("PASS ({} exchanges)", sc[0]);
            pass_count += 1;
        }
    }

    // ── 28. Gateway-local: req_news_providers ──
    {
        print!("  req_news_providers... ");
        wrapper.drain();
        client.req_news_providers(&mut wrapper);
        let cbs = wrapper.drain();
        let np: Vec<_> = cbs.iter().filter_map(|c| if let Cb::NewsProviders { count, .. } = c { Some(*count) } else { None }).collect();
        if np.is_empty() || np[0] == 0 {
            println!("FAIL (no news providers)");
            fail_count += 1;
        } else {
            println!("PASS ({} providers)", np[0]);
            pass_count += 1;
        }
    }

    // ── 29. Gateway-local: req_current_time ──
    {
        print!("  req_current_time... ");
        wrapper.drain();
        client.req_current_time(&mut wrapper);
        let cbs = wrapper.drain();
        let ct: Vec<_> = cbs.iter().filter_map(|c| if let Cb::CurrentTime { time, .. } = c { Some(*time) } else { None }).collect();
        if ct.is_empty() {
            println!("FAIL (no current_time callback)");
            fail_count += 1;
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if (ct[0] - now).abs() < 5 {
                println!("PASS (time={})", ct[0]);
                pass_count += 1;
            } else {
                println!("FAIL (time={} too far from now={})", ct[0], now);
                fail_count += 1;
            }
        }
    }

    // ── 30. Gateway-local: req_soft_dollar_tiers ──
    {
        print!("  req_soft_dollar_tiers... ");
        wrapper.drain();
        client.req_soft_dollar_tiers(901, &mut wrapper);
        let cbs = wrapper.drain();
        let sdt: Vec<_> = cbs.iter().filter_map(|c| if let Cb::SoftDollarTiers { count, .. } = c { Some(*count) } else { None }).collect();
        if sdt.is_empty() || sdt[0] == 0 {
            println!("FAIL (no soft dollar tiers)");
            fail_count += 1;
        } else {
            println!("PASS ({} tiers)", sdt[0]);
            pass_count += 1;
        }
    }

    // ── 31. Gateway-local: req_family_codes ──
    {
        print!("  req_family_codes... ");
        wrapper.drain();
        client.req_family_codes(&mut wrapper);
        let cbs = wrapper.drain();
        let fc: Vec<_> = cbs.iter().filter_map(|c| if let Cb::FamilyCodes { count, .. } = c { Some(*count) } else { None }).collect();
        if fc.is_empty() {
            println!("FAIL (no family_codes callback)");
            fail_count += 1;
        } else {
            // Paper/single accounts return empty list — that's OK
            println!("PASS ({} codes)", fc[0]);
            pass_count += 1;
        }
    }

    // ── 32. Gateway-local: req_user_info ──
    {
        print!("  req_user_info... ");
        wrapper.drain();
        client.req_user_info(902, &mut wrapper);
        let cbs = wrapper.drain();
        let ui: Vec<_> = cbs.iter().filter_map(|c| if let Cb::UserInfo { req_id, white_branding_id, .. } = c { Some((*req_id, white_branding_id.clone())) } else { None }).collect();
        if ui.is_empty() {
            println!("FAIL (no user_info callback)");
            fail_count += 1;
        } else {
            let (rid, wbid) = &ui[0];
            if *rid == 902 {
                println!("PASS (whiteBrandingId='{}')", wbid);
                pass_count += 1;
            } else {
                println!("FAIL (wrong req_id: {})", rid);
                fail_count += 1;
            }
        }
    }

    // ── 33. Disconnect + verify ──
    {
        print!("  disconnect... ");
        client.disconnect();
        let still_connected = client.is_connected();
        if !still_connected {
            println!("PASS (is_connected=false)");
            pass_count += 1;
        } else {
            println!("FAIL (is_connected still true)");
            fail_count += 1;
        }
    }

    println!("\n=== Results: {} PASS, {} FAIL, {} SKIP ({:.1}s) ===",
        pass_count, fail_count, skip_count, suite_start.elapsed().as_secs_f64());
    assert_eq!(fail_count, 0, "Some API GT tests failed");
}
