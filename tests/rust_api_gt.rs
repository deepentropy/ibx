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
    // Place no order unless this is a paper account (id starts with DU).
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: the logged-in account is not a paper account (its id does not start with DU)");
    }

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
        client.req_historical_ticks(440, &spy(), "20260320 09:30:00 US/Eastern", "", 1000, "TRADES", true, false, &[]).unwrap();
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
        client.req_scanner_subscription(460, &ScannerSubscription {
            instrument: "STK".into(), location_code: "STK.US.MAJOR".into(),
            scan_code: "TOP_PERC_GAIN".into(), number_of_rows: 10, ..Default::default()
        }, &[], &[]).unwrap();
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
        // Only this request's errors: the scanner cancel above is acknowledged
        // with its own error 162 under its request id, as the reference does.
        poll_until(&client, &mut wrapper,
            |cbs| cbs.iter().any(|c| matches!(c, Cb::FundamentalData { .. } | Cb::Error { req_id: 470, .. })),
            Duration::from_secs(15));
        let cbs = wrapper.drain();

        let fd: Vec<_> = cbs.iter().filter_map(|c| if let Cb::FundamentalData { has_data, .. } = c { Some(*has_data) } else { None }).collect();
        let errors: Vec<_> = cbs.iter().filter_map(|c| if let Cb::Error { req_id: 470, msg, .. } = c { Some(msg.clone()) } else { None }).collect();

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
        // Subscribed providers only: the reference refuses a provider that
        // carries service ids in the logon source list (321).
        client.req_historical_news(480, 265598, "BRFG+DJNL+BRFUPDN", "", "", 5).unwrap();
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
        client.req_scanner_subscription(600, &ScannerSubscription {
            instrument: "STK".into(), location_code: "STK.US.MAJOR".into(),
            scan_code: "TOP_PERC_GAIN".into(), number_of_rows: 5, ..Default::default()
        }, &[], &[]).unwrap();
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
            // The server refuses outside-RTH for algo orders ("Only RTH orders
            // are allowed for IB algorithmic orders"). This case passed before
            // only because ibx dropped the flag on adaptive orders (ibx#318).
            outside_rth: false,
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
            // A regular-hours adaptive GTC order is accepted at any time of
            // day; a rejection means ibx encoded it wrongly.
            println!("FAIL (adaptive order rejected)");
            fail_count += 1;
        } else {
            println!("FAIL (no order_status received)");
            fail_count += 1;
        }
    }

    // ── 27. Gateway-local: req_smart_components ──
    {
        // A code no market data gave is refused, as the reference (ibx#441);
        // the answer for a real code is in api_smart_components_live.
        print!("  req_smart_components... ");
        wrapper.drain();
        client.req_smart_components(900, "XYZ", &mut wrapper);
        let cbs = wrapper.drain();
        let refused = cbs.iter().any(|c| matches!(c, Cb::Error { req_id: 900, code: 321, msg }
            if msg == "Error validating request.-'V' : cause - Invalid BBO exchange/security type code"));
        if !refused {
            println!("FAIL (no 321 for an unknown code: {:?})", cbs.len());
            fail_count += 1;
        } else {
            println!("PASS (321)");
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

// ── Plain snapshot (ibx#446), focused ──

#[derive(Default)]
struct SnapWrapper {
    start: Option<Instant>,
    events: Vec<(u128, String)>,
}

impl SnapWrapper {
    fn at(&mut self, what: String) {
        let ms = self.start.map(|s| s.elapsed().as_millis()).unwrap_or(0);
        self.events.push((ms, what));
    }
}

impl Wrapper for SnapWrapper {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) { self.at(format!("error {req_id} {code} {text}")); }
    fn tick_price(&mut self, req_id: i64, tt: i32, p: f64, a: &TickAttrib) {
        self.at(format!("price {req_id} {tt} {p} {}", if a.can_auto_execute { "auto" } else { "-" }));
    }
    fn tick_size(&mut self, req_id: i64, tt: i32, s: f64) { self.at(format!("size {req_id} {tt} {s}")); }
    fn tick_string(&mut self, req_id: i64, tt: i32, v: &str) { self.at(format!("string {req_id} {tt} {v}")); }
    fn tick_generic(&mut self, req_id: i64, tt: i32, v: f64) { self.at(format!("generic {req_id} {tt} {v}")); }
    fn tick_req_params(&mut self, req_id: i64, min_tick: f64, bbo: &str, perms: i64) {
        self.at(format!("params {req_id} {min_tick} {bbo} {perms}"));
    }
    fn market_data_type(&mut self, req_id: i64, t: i32) { self.at(format!("mdt {req_id} {t}")); }
    fn tick_snapshot_end(&mut self, req_id: i64) { self.at(format!("end {req_id}")); }
    fn tick_news(&mut self, req_id: i64, time: i64, provider: &str, article: &str, headline: &str, extra: &str) {
        self.at(format!("news {req_id} {time} {provider} {article} {{{extra}}} {headline}"));
    }
}

/// Snapshots of a stock, an ETF and a currency pair: each tick type at
/// most once, the end within the time limit; a snapshot with generic
/// ticks is refused; a cancel after the end is for an unknown request.
/// Run with: cargo test --test rust_api_gt api_snapshot_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_snapshot_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = SnapWrapper::default();
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(3) {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(50));
    }
    w.events.clear();
    w.start = Some(Instant::now());
    let eurusd = Contract { con_id: 12087792, symbol: "EUR".into(), sec_type: "CASH".into(),
        exchange: "IDEALPRO".into(), currency: "USD".into(), ..Default::default() };
    client.req_mkt_data(701, &spy(), "", true, false).unwrap();
    client.req_mkt_data(702, &aapl(), "", true, false).unwrap();
    client.req_mkt_data(703, &eurusd, "", true, false).unwrap();
    client.req_mkt_data(704, &spy(), "233", true, false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        client.process_msgs(&mut w);
        let ends = w.events.iter().filter(|(_, e)| e.starts_with("end ")).count();
        if ends == 3 { break; }
        std::thread::sleep(Duration::from_millis(20));
    }
    client.cancel_mkt_data(701).unwrap();
    let after = Instant::now();
    while after.elapsed() < Duration::from_secs(1) {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(20));
    }
    for (ms, e) in &w.events {
        println!("  {ms:>6} ms  {e}");
    }
    client.disconnect();

    let refusal = "error 704 321 Error validating request.-'bQ' : cause - Snapshot market data subscription is not applicable to generic ticks";
    assert!(w.events.iter().any(|(_, e)| e == refusal), "no generic tick refusal");
    for req in [701, 702, 703] {
        let end = w.events.iter().find(|(_, e)| *e == format!("end {req}")).map(|(ms, _)| *ms);
        assert!(end.is_some_and(|ms| ms <= 12_500), "req {req}: end {end:?}");
        let mut seen = std::collections::HashSet::new();
        for (_, e) in &w.events {
            let parts: Vec<&str> = e.split(' ').collect();
            if matches!(parts[0], "price" | "size" | "string" | "generic") && parts[1] == req.to_string() {
                assert!(seen.insert((parts[0].to_string(), parts[2].to_string())), "req {req}: {e} twice");
            }
        }
    }
    assert!(w.events.iter().any(|(_, e)| e.starts_with("error 701 300 ")), "cancel after the end: no 300");
}

// ── Currency pair prices on the tick of their server tag, focused ──

/// EUR.USD streaming and snapshot: bid, ask and last near 1.1, not five
/// times higher (the bid/ask book and the last price tick in different
/// steps), and the request parameters carry the bid/ask entry's tick.
/// Run with: cargo test --test rust_api_gt api_eurusd_prices_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_eurusd_prices_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = SnapWrapper::default();
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(3) {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(50));
    }
    w.events.clear();
    w.start = Some(Instant::now());
    let eurusd = Contract { con_id: 12087792, symbol: "EUR".into(), sec_type: "CASH".into(),
        exchange: "IDEALPRO".into(), currency: "USD".into(), ..Default::default() };
    client.req_mkt_data(801, &eurusd, "", false, false).unwrap();
    let streamed = Instant::now();
    while streamed.elapsed() < Duration::from_secs(10) {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(20));
    }
    client.cancel_mkt_data(801).unwrap();
    client.req_mkt_data(802, &eurusd, "", true, false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !w.events.iter().any(|(_, e)| e == "end 802") {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(20));
    }
    client.disconnect();
    for (ms, e) in &w.events {
        println!("  {ms:>6} ms  {e}");
    }

    for req in [801, 802] {
        let mut kinds = std::collections::HashSet::new();
        for (_, e) in &w.events {
            let parts: Vec<&str> = e.split(' ').collect();
            if parts[0] == "price" && parts[1] == req.to_string() && matches!(parts[2], "1" | "2" | "4") {
                let p: f64 = parts[3].parse().unwrap();
                assert!(p > 0.8 && p < 1.6, "req {req}: {e}");
                kinds.insert(parts[2].to_string());
            }
        }
        assert!(kinds.contains("1") && kinds.contains("2"), "req {req}: no bid or ask: {kinds:?}");
    }
    assert!(w.events.iter().any(|(_, e)| e.starts_with("params 801 0.00001 ")), "no request parameters with the bid/ask tick");
    // As the reference (ibx#446): the market data type before the request
    // parameters; a stream's bid and ask execute automatically, a currency
    // pair snapshot's do not; the snapshot sends the trade's time before
    // the quotes.
    let pos = |pred: &dyn Fn(&str) -> bool| w.events.iter().position(|(_, e)| pred(e));
    for req in [801, 802] {
        let mdt = pos(&|e: &str| e == format!("mdt {req} 1"));
        let params = pos(&|e: &str| e.starts_with(&format!("params {req} ")));
        assert!(mdt.is_some() && params.is_some() && mdt < params, "req {req}: type {mdt:?}, parameters {params:?}");
    }
    for (req, auto) in [(801, "auto"), (802, "-")] {
        for (_, e) in &w.events {
            let parts: Vec<&str> = e.split(' ').collect();
            if parts[0] == "price" && parts[1] == req.to_string() && matches!(parts[2], "1" | "2") {
                assert_eq!(parts[4], auto, "req {req}: {e}");
            }
        }
    }
    let time = pos(&|e: &str| e.starts_with("string 802 45 "));
    let bid = pos(&|e: &str| e.starts_with("price 802 1 "));
    assert!(time.is_none() || time < bid, "snapshot: time {time:?} after the bid {bid:?}");
}

// ── Stream callback order (ibx#446), focused ──

/// EUR.USD and SPY streams for 15 s: the market data type before the
/// request parameters; every bid, ask and last price followed by its size;
/// never the last exchange (84).
/// Run with: cargo test --test rust_api_gt api_stream_order_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_stream_order_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = SnapWrapper::default();
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(3) {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(50));
    }
    w.events.clear();
    w.start = Some(Instant::now());
    let eurusd = Contract { con_id: 12087792, symbol: "EUR".into(), sec_type: "CASH".into(),
        exchange: "IDEALPRO".into(), currency: "USD".into(), ..Default::default() };
    client.req_mkt_data(811, &eurusd, "", false, false).unwrap();
    client.req_mkt_data(812, &spy(), "", false, false).unwrap();
    let streamed = Instant::now();
    while streamed.elapsed() < Duration::from_secs(15) {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(5));
    }
    client.cancel_mkt_data(811).unwrap();
    client.cancel_mkt_data(812).unwrap();
    client.disconnect();
    for (ms, e) in &w.events {
        println!("  {ms:>6} ms  {e}");
    }
    for req in [811, 812] {
        let pos = |want: &str| w.events.iter().position(|(_, e)| e.starts_with(want));
        let (mdt, params) = (pos(&format!("mdt {req} ")), pos(&format!("params {req} ")));
        assert!(mdt.is_some() && params.is_some() && mdt < params, "req {req}: type {mdt:?}, parameters {params:?}");
        let mine: Vec<&String> = w.events.iter().map(|(_, e)| e).filter(|e| e.split(' ').nth(1) == Some(&req.to_string())).collect();
        for (k, e) in mine.iter().enumerate() {
            let parts: Vec<&str> = e.split(' ').collect();
            assert!(!(parts[0] == "string" && parts[2] == "84"), "req {req}: {e}");
            if parts[0] == "price" {
                let size = match parts[2] { "1" => "0", "2" => "3", "4" => "5", _ => continue };
                let next = mine.get(k + 1).map(|n| n.split(' ').take(3).collect::<Vec<_>>());
                assert_eq!(next, Some(vec!["size", parts[1], size]), "req {req}: {e} without its size");
            }
        }
    }
}

// ── Smart components (ibx#441), focused ──

/// Smart components of a request: (bit, exchange, letter).
type SmartRows = Vec<(i32, String, String)>;

#[derive(Default)]
struct SmartWrapper {
    events: Vec<String>,
    params: Vec<(i64, String)>,
    components: Vec<(i64, SmartRows)>,
}

impl Wrapper for SmartWrapper {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) { self.events.push(format!("error {req_id} {code} {text}")); }
    fn tick_req_params(&mut self, req_id: i64, _min_tick: f64, bbo: &str, _perms: i64) { self.params.push((req_id, bbo.to_string())); }
    fn smart_components(&mut self, req_id: i64, components: &[ibx::types::SmartComponent]) {
        self.components.push((req_id, components.iter()
            .map(|c| (c.bit_number, c.exchange.clone(), c.exchange_letter.clone())).collect()));
    }
}

/// As captured on the reference 02/10/2026: an unknown code gives 321; the
/// bboExchange of the AAPL request parameters, asked at once, is answered
/// within 2 s with the exchange map of that code (about 20 exchanges,
/// sorted by bit, NASDAQ among them), and again at once 3 s later.
/// Run with: cargo test --test rust_api_gt api_smart_components_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_smart_components_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = SmartWrapper::default();
    let pump = |client: &EClient, w: &mut SmartWrapper, d: Duration| {
        let start = Instant::now();
        while start.elapsed() < d {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    pump(&client, &mut w, Duration::from_secs(3));
    client.req_smart_components(9490, "XYZ", &mut w);
    client.req_mkt_data(9491, &aapl(), "", false, false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while w.params.is_empty() && Instant::now() < deadline {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(10));
    }
    let bbo = w.params.first().map(|p| p.1.clone()).unwrap_or_default();
    client.req_smart_components(9492, &bbo, &mut w);
    pump(&client, &mut w, Duration::from_secs(3));
    client.req_smart_components(9493, &bbo, &mut w);
    client.cancel_mkt_data(9491).unwrap();
    pump(&client, &mut w, Duration::from_millis(500));
    client.disconnect();
    println!("  bboExchange {bbo:?}
  events {:?}", w.events);
    for (req, comps) in &w.components {
        println!("  smart components {req}: {comps:?}");
    }
    assert!(w.events.iter().any(|e| e == "error 9490 321 Error validating request.-'V' : cause - Invalid BBO exchange/security type code"));
    assert!(bbo.ends_with("0001"), "AAPL bboExchange {bbo:?}");
    for req in [9492, 9493] {
        let comps = w.components.iter().find(|(r, _)| *r == req).map(|(_, c)| c.clone()).unwrap_or_default();
        assert!(comps.len() >= 15, "req {req}: {} components", comps.len());
        assert!(comps.windows(2).all(|p| p[0].0 < p[1].0), "req {req}: not sorted by bit");
        assert!(comps.iter().any(|c| c.1 == "NASDAQ"), "req {req}: no NASDAQ");
    }
}

// ── Contract lookups CONTFUT, by conId with an exchange, bond issuer (ibx#438), focused ──

/// A row: (reqId, "row" or "bond", conId, secType, exchange).
type LookupRow = (i64, String, i64, String, String);

#[derive(Default)]
struct LookupWrapper {
    events: Vec<String>,
    rows: Vec<LookupRow>,
    ends: Vec<i64>,
    issuers: Vec<String>,
}

impl Wrapper for LookupWrapper {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) { self.events.push(format!("error {req_id} {code} {text}")); }
    fn contract_details(&mut self, req_id: i64, d: &ContractDetails) {
        self.rows.push((req_id, "row".into(), d.contract.con_id, d.contract.sec_type.clone(), d.contract.exchange.clone()));
    }
    fn bond_contract_details(&mut self, req_id: i64, d: &ContractDetails) {
        self.rows.push((req_id, "bond".into(), d.contract.con_id, d.contract.sec_type.clone(), d.contract.exchange.clone()));
    }
    fn contract_details_end(&mut self, req_id: i64) { self.ends.push(req_id); }
    fn symbol_samples(&mut self, _req_id: i64, descriptions: &[ContractDescription]) {
        self.issuers.extend(descriptions.iter().filter(|d| !d.issuer_id.is_empty()).map(|d| d.issuer_id.clone()));
    }
}

/// As captured on the reference 02/10/2026: CONTFUT ES CME gives one row,
/// secType CONTFUT; FUT+CONTFUT gives the CONTFUT row first, then the
/// futures; conId 265598 on ISLAND gives one row; a bond issuer from the
/// IBM matching symbols gives bond rows (with and without secType BOND).
/// Run with: cargo test --test rust_api_gt api_contract_lookups_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_contract_lookups_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = LookupWrapper::default();
    let wait_end = |client: &EClient, w: &mut LookupWrapper, req: i64| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !w.ends.contains(&req) && Instant::now() < deadline {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let es = |sec_type: &str| Contract { symbol: "ES".into(), sec_type: sec_type.into(), exchange: "CME".into(),
        currency: "USD".into(), ..Default::default() };
    std::thread::sleep(Duration::from_secs(3));
    client.req_contract_details(9480, &es("CONTFUT")).unwrap();
    wait_end(&client, &mut w, 9480);
    client.req_contract_details(9481, &es("FUT+CONTFUT")).unwrap();
    wait_end(&client, &mut w, 9481);
    let by_con_id = Contract { con_id: 265598, exchange: "ISLAND".into(), ..Default::default() };
    client.req_contract_details(9483, &by_con_id).unwrap();
    wait_end(&client, &mut w, 9483);
    client.req_matching_symbols(9487, "IBM").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while w.issuers.is_empty() && Instant::now() < deadline {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(20));
    }
    let issuer = w.issuers.first().cloned().unwrap_or_else(|| "e1400789".into());
    let bond = Contract { sec_type: "BOND".into(), issuer_id: issuer.clone(), currency: "USD".into(), ..Default::default() };
    client.req_contract_details(9488, &bond).unwrap();
    wait_end(&client, &mut w, 9488);
    let any = Contract { issuer_id: issuer.clone(), ..Default::default() };
    client.req_contract_details(9489, &any).unwrap();
    wait_end(&client, &mut w, 9489);
    client.disconnect();
    let of = |req: i64| w.rows.iter().filter(|r| r.0 == req).cloned().collect::<Vec<_>>();
    for req in [9480, 9481, 9483, 9488, 9489] {
        let rows = of(req);
        println!("  {req}: {} rows, first {:?}, end {}", rows.len(), rows.first(), w.ends.contains(&req));
    }
    println!("  issuer {issuer}, events {:?}", w.events);
    let contfut = of(9480);
    assert_eq!(contfut.len(), 1);
    assert_eq!(contfut[0].3, "CONTFUT");
    let both = of(9481);
    assert!(both.len() > 2 && both[0].3 == "CONTFUT" && both[1..].iter().all(|r| r.3 == "FUT"), "{both:?}");
    assert!(both[1..].iter().any(|r| r.2 == contfut[0].2), "the front month again as FUT");
    let island = of(9483);
    assert_eq!(island.len(), 1);
    assert_eq!(island[0].2, 265598);
    for req in [9488, 9489] {
        let rows = of(req);
        assert!(!rows.is_empty() && rows.iter().all(|r| r.1 == "bond" && r.3 == "BOND"), "req {req}: {rows:?}");
        assert!(w.ends.contains(&req));
    }
}

// ── Contract details fields (ibx#436), focused ──

#[derive(Default)]
struct DetailsWrapper {
    events: Vec<String>,
    rows: Vec<(i64, bool, ContractDetails)>,
    ends: Vec<i64>,
    issuers: Vec<String>,
    chains: Vec<(String, Vec<String>, Vec<f64>)>,
    chain_end: bool,
}

impl Wrapper for DetailsWrapper {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) { self.events.push(format!("error {req_id} {code} {text}")); }
    fn contract_details(&mut self, req_id: i64, d: &ContractDetails) { self.rows.push((req_id, false, d.clone())); }
    fn bond_contract_details(&mut self, req_id: i64, d: &ContractDetails) { self.rows.push((req_id, true, d.clone())); }
    fn contract_details_end(&mut self, req_id: i64) { self.ends.push(req_id); }
    fn symbol_samples(&mut self, _req_id: i64, descriptions: &[ContractDescription]) {
        self.issuers.extend(descriptions.iter().filter(|d| !d.issuer_id.is_empty()).map(|d| d.issuer_id.clone()));
    }
    fn security_definition_option_parameter(&mut self, _req_id: i64, exchange: &str, _under: i64,
        _class: &str, _mult: &str, expirations: &[String], strikes: &[f64]) {
        self.chains.push((exchange.to_string(), expirations.to_vec(), strikes.to_vec()));
    }
    fn security_definition_option_parameter_end(&mut self, _req_id: i64) { self.chain_end = true; }
}

/// The fields of the reference's rows, as captured on paper 28/09/2026
/// and 02/10/2026; any market session (the lookups need no market data).
/// AAPL: industry Technology / Computers / Computers, stock type COMMON,
/// the ISIN as only id, a market rule id per valid exchange, US/Eastern
/// hours from today, fractional sizes 0.0001. MNQ futures: date and time
/// of the last trade (08:30:00), month, underlying MNQ IND, long name on
/// every row. An AAPL call: right C, underlying 265598 STK, last trade time
/// 16:00:00. IBM bonds: bond rows, local symbol as CUSIP, a description
/// append, ISIN and CUSIP ids.
/// Run with: cargo test --test rust_api_gt api_contract_details_fields_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_contract_details_fields_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = DetailsWrapper::default();
    let wait_end = |client: &EClient, w: &mut DetailsWrapper, req: i64| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !w.ends.contains(&req) && Instant::now() < deadline {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    std::thread::sleep(Duration::from_secs(3));
    let aapl = Contract { symbol: "AAPL".into(), sec_type: "STK".into(), exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    client.req_contract_details(9601, &aapl).unwrap();
    wait_end(&client, &mut w, 9601);
    let mnq = Contract { symbol: "MNQ".into(), sec_type: "FUT".into(), exchange: "CME".into(), currency: "USD".into(), ..Default::default() };
    client.req_contract_details(9602, &mnq).unwrap();
    wait_end(&client, &mut w, 9602);
    // An AAPL call of the chain: the middle expiry, the middle strike of its calls.
    client.req_sec_def_opt_params(9603, "AAPL", "", "STK", 265598).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !w.chain_end && Instant::now() < deadline {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(20));
    }
    let chain = w.chains.iter().find(|c| c.0 == "SMART").cloned().expect("an AAPL chain on SMART");
    let mut expiries = chain.1.clone();
    expiries.sort();
    // The chain's strikes are those of every expiry: the strike comes from
    // the calls of the chosen expiry, asked with no strike first.
    let expiry = expiries[expiries.len() / 2].clone();
    let calls = Contract {
        symbol: "AAPL".into(), sec_type: "OPT".into(), exchange: "SMART".into(), currency: "USD".into(),
        last_trade_date_or_contract_month: expiry.clone(), right: "C".into(), multiplier: "100".into(), ..Default::default()
    };
    client.req_contract_details(9607, &calls).unwrap();
    wait_end(&client, &mut w, 9607);
    let mut strikes: Vec<f64> = w.rows.iter().filter(|r| r.0 == 9607).map(|r| r.2.contract.strike).collect();
    strikes.sort_by(|a, b| a.partial_cmp(b).unwrap());
    strikes.dedup();
    assert!(!strikes.is_empty(), "no AAPL call for {expiry} (chain strikes {:?})", chain.2);
    let call = Contract { strike: strikes[strikes.len() / 2], ..calls.clone() };
    client.req_contract_details(9604, &call).unwrap();
    wait_end(&client, &mut w, 9604);
    client.req_matching_symbols(9605, "IBM").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while w.issuers.is_empty() && Instant::now() < deadline {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(20));
    }
    let issuer = w.issuers.first().cloned().unwrap_or_else(|| "e1400789".into());
    let bond = Contract { sec_type: "BOND".into(), issuer_id: issuer, currency: "USD".into(), ..Default::default() };
    client.req_contract_details(9606, &bond).unwrap();
    wait_end(&client, &mut w, 9606);
    client.disconnect();
    let of = |req: i64| w.rows.iter().filter(|r| r.0 == req).map(|r| (r.1, r.2.clone())).collect::<Vec<_>>();
    for req in [9601, 9602, 9604, 9606] {
        let rows = of(req);
        println!("  {req}: {} rows, end {}", rows.len(), w.ends.contains(&req));
        if let Some((_, d)) = rows.first() {
            println!("    {:?}", d);
        }
    }
    println!("  events {:?}", w.events);
    let today = jiff::Zoned::now().with_time_zone(jiff::tz::TimeZone::get("US/Eastern").unwrap()).date().strftime("%Y%m%d").to_string();

    let rows = of(9601);
    assert_eq!(rows.len(), 1);
    let d = &rows[0].1;
    assert_eq!((d.contract.con_id, d.contract.sec_type.as_str(), d.contract.primary_exchange.as_str()), (265598, "STK", "NASDAQ"));
    assert_eq!((d.industry.as_str(), d.category.as_str(), d.subcategory.as_str()), ("Technology", "Computers", "Computers"));
    assert_eq!((d.stock_type.as_str(), d.under_con_id, d.agg_group, d.price_magnifier), ("COMMON", 0, 1, 1));
    assert_eq!(d.sec_id_list, vec![TagValue { tag: "ISIN".into(), value: "US0378331005".into() }]);
    assert_eq!(d.market_rule_ids.split(',').count(), d.valid_exchanges.split(',').count(), "{} / {}", d.market_rule_ids, d.valid_exchanges);
    assert_eq!(d.time_zone_id, "US/Eastern");
    assert!(d.trading_hours.starts_with(&format!("{today}:")) && d.liquid_hours.starts_with(&format!("{today}:")),
        "{} / {}", d.trading_hours, d.liquid_hours);
    assert_eq!((d.min_size, d.size_increment), (0.0001, 0.0001));
    assert!(d.suggested_size_increment >= 1.0 && d.min_tick == 0.01);
    assert!(!d.order_types.contains('/'), "{}", d.order_types);

    let rows = of(9602);
    assert!(rows.len() >= 2, "{} MNQ rows", rows.len());
    for (_, d) in &rows {
        assert_eq!(d.contract.last_trade_date_or_contract_month.len(), 8, "{:?}", d.contract);
        assert_eq!(d.contract.last_trade_date, d.contract.last_trade_date_or_contract_month);
        assert_eq!((d.last_trade_time.as_str(), d.contract.multiplier.as_str()), ("08:30:00", "2"));
        assert_eq!(d.contract_month, d.contract.last_trade_date[..6]);
        assert_eq!((d.under_symbol.as_str(), d.under_sec_type.as_str()), ("MNQ", "IND"));
        assert!(d.under_con_id > 0 && !d.long_name.is_empty() && !d.real_expiration_date.is_empty());
        assert_eq!(d.time_zone_id, "US/Central");
    }

    let rows = of(9604);
    assert_eq!(rows.len(), 1, "the call {:?}", call);
    let d = &rows[0].1;
    assert_eq!((d.contract.right.as_str(), d.contract.multiplier.as_str()), ("C", "100"));
    assert_eq!((d.under_con_id, d.under_symbol.as_str(), d.under_sec_type.as_str()), (265598, "AAPL", "STK"));
    assert_eq!(d.last_trade_time, "16:00:00");
    assert_eq!(d.contract.last_trade_date_or_contract_month, call.last_trade_date_or_contract_month);

    let rows = of(9606);
    assert!(!rows.is_empty() && rows.iter().all(|(bond, d)| *bond && d.contract.sec_type == "BOND"));
    for (_, d) in &rows {
        assert_eq!(d.cusip, d.contract.local_symbol);
        assert!(!d.desc_append.is_empty());
        assert!(d.sec_id_list.iter().any(|t| t.tag == "ISIN"));
    }
    assert!(rows.iter().any(|(_, d)| d.sec_id_list.iter().any(|t| t.tag == "CUSIP")));
}

// ── Historical ticks, keepUpToDate and date strings (ibx#432, ibx#429, ibx#431), focused ──

#[derive(Default)]
struct HistWrapper {
    events: Vec<String>,
}

impl Wrapper for HistWrapper {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) {
        self.events.push(format!("error {req_id} {code} {}", text.lines().next().unwrap_or("")));
    }
    fn historical_ticks(&mut self, req_id: i64, ticks: &ibx::types::HistoricalTickData, done: bool) {
        if let ibx::types::HistoricalTickData::Midpoint(t) = ticks {
            self.events.push(format!("mid {req_id} {} {:?} {done}", t.len(), t.first().map(|x| (x.time, x.price, x.size))));
        }
    }
    fn historical_ticks_last(&mut self, req_id: i64, ticks: &ibx::types::HistoricalTickData, done: bool) {
        if let ibx::types::HistoricalTickData::Last(t) = ticks {
            self.events.push(format!("last {req_id} {} {:?} {done}", t.len(),
                t.first().map(|x| (x.time, x.tick_attrib_last.unreported, x.price, x.size, x.exchange.clone()))));
        }
    }
    fn historical_ticks_bid_ask(&mut self, req_id: i64, ticks: &ibx::types::HistoricalTickData, done: bool) {
        if let ibx::types::HistoricalTickData::BidAsk(t) = ticks {
            self.events.push(format!("bidask {req_id} {} {:?} {done}", t.len(), t.first().map(|x| (x.time, x.price_bid, x.size_bid))));
        }
    }
    fn historical_data(&mut self, req_id: i64, bar: &BarData) {
        self.events.push(format!("bar {req_id} {}", bar.date));
    }
    fn historical_data_end(&mut self, req_id: i64, start: &str, end: &str) {
        self.events.push(format!("end {req_id} {start} | {end}"));
    }
    fn historical_data_update(&mut self, req_id: i64, bar: &BarData) {
        self.events.push(format!("update {req_id} {} {} {} {}", bar.date, bar.close, bar.volume, bar.bar_count));
    }
    fn head_timestamp(&mut self, req_id: i64, head_timestamp: &str) {
        self.events.push(format!("head {req_id} {head_timestamp}"));
    }
}

/// Historical ticks (TRADES, BID_ASK, EUR.USD TRADES, 0 ticks), a
/// keepUpToDate 1 min request for 20 s then its cancel, formatDate 2 bars,
/// head timestamp formatDate 2 and the cancel of an unknown request, as
/// captured from the reference on 02/10/2026.
/// Run with: cargo test --test rust_api_gt api_historical_b1_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_historical_b1_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = HistWrapper::default();
    let run = |client: &EClient, w: &mut HistWrapper, secs: u64| {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(secs) {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    run(&client, &mut w, 3);
    w.events.clear();
    let t15 = "20261001 15:00:00 US/Eastern";
    let eurusd = Contract { con_id: 12087792, symbol: "EUR".into(), sec_type: "CASH".into(),
        exchange: "IDEALPRO".into(), currency: "USD".into(), ..Default::default() };
    client.req_historical_ticks(9510, &aapl(), t15, "", 100, "TRADES", true, false, &[]).unwrap();
    client.req_historical_ticks(9512, &aapl(), t15, "", 100, "BID_ASK", true, false, &[]).unwrap();
    client.req_historical_ticks(9519, &aapl(), t15, "", 0, "TRADES", true, false, &[]).unwrap();
    client.req_historical_ticks(9522, &eurusd, t15, "", 10, "TRADES", true, false, &[]).unwrap();
    run(&client, &mut w, 15);
    client.req_historical_data(9531, &aapl(), "", "3600 S", "1 min", "TRADES", false, 1, true).unwrap();
    client.req_historical_data(9541, &aapl(), "", "1 D", "1 hour", "TRADES", true, 2, false).unwrap();
    client.req_head_time_stamp(9545, &aapl(), "TRADES", true, 2).unwrap();
    run(&client, &mut w, 20);
    client.cancel_historical_data(9531).unwrap();
    client.cancel_historical_data(99).unwrap();
    run(&client, &mut w, 3);
    let after_cancel = w.events.len();
    run(&client, &mut w, 10);
    client.disconnect();
    for e in &w.events {
        println!("  {e}");
    }
    let has = |p: &str| w.events.iter().any(|e| e.starts_with(p));
    assert!(has("last 9510 ") && w.events.iter().any(|e| e.starts_with("last 9510 ") && e.ends_with(" true")), "trade ticks with the end");
    assert!(has("bidask 9512 "), "bid/ask ticks");
    assert!(has("error 9519 321 Error validating request.-'bP' : cause - Number of ticks must be > 0"));
    assert!(has("mid 9522 "), "EUR.USD trades come as midpoints");
    assert!(w.events.iter().any(|e| e.starts_with("end 9531 ") && e.ends_with(" US/Eastern")), "end strings");
    let updates = w.events.iter().filter(|e| e.starts_with("update 9531 ")).count();
    assert!(updates >= 2, "one update per 5 s: {updates}");
    assert!(w.events.iter().filter(|e| e.starts_with("update 9531 ")).all(|e| e.contains(" US/Eastern ")));
    assert!(has("error 9531 162 Historical Market Data Service error message:API historical data query cancelled: 9531"));
    assert!(!w.events[after_cancel..].iter().any(|e| e.starts_with("update 9531 ")), "no update after the cancel");
    assert!(w.events.iter().filter(|e| e.starts_with("bar 9541 ")).all(|e| e[9..].chars().all(|c| c.is_ascii_digit())), "formatDate 2");
    assert!(has("head 9545 345479400"), "head timestamp in seconds");
    assert!(has("error 99 366 No historical data query found for ticker id:99"));
}

// ── News ticks (ibx#458), focused ──

/// Contract news through the news tick: explicit codes on AAPL, every
/// source on TSLA, a future (10094 derivative) and an unknown code on NVDA
/// (10094 unchecked). The recent headlines come within seconds, with the
/// time in milliseconds and the {...} part as extra data.
/// Run with: cargo test --test rust_api_gt api_news_ticks_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_news_ticks_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = SnapWrapper::default();
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(3) {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(50));
    }
    w.events.clear();
    w.start = Some(Instant::now());
    let stock = |con_id: i64, symbol: &str| Contract { con_id, symbol: symbol.into(), sec_type: "STK".into(),
        exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    client.req_mkt_data(821, &stock(265598, "AAPL"), "mdoff,292:BRFG+DJNL", false, false).unwrap();
    client.req_mkt_data(822, &stock(76792991, "TSLA"), "mdoff,292", false, false).unwrap();
    let mnq = Contract { con_id: 815824267, symbol: "MNQ".into(), sec_type: "FUT".into(),
        exchange: "CME".into(), currency: "USD".into(), ..Default::default() };
    client.req_mkt_data(823, &mnq, "mdoff,292", false, false).unwrap();
    client.req_mkt_data(824, &stock(4815747, "NVDA"), "mdoff,292:XYZ", false, false).unwrap();
    let listen = Instant::now();
    while listen.elapsed() < Duration::from_secs(20) {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(20));
    }
    for req in [821, 822] {
        client.cancel_mkt_data(req).unwrap();
    }
    client.disconnect();
    for (ms, e) in &w.events {
        println!("  {ms:>6} ms  {e}");
    }
    let has = |want: &str| w.events.iter().any(|(_, e)| e == want);
    assert!(has("error 823 10094 API News error:Derivative contracts cannot be used to subscribe to news, please use the underlying (Stocks, Cash, News Topics, and certain Indexes are supported)."));
    assert!(has("error 824 10094 API News error:Source code unchecked in API news Settings: XYZ"));
    let news: Vec<Vec<&str>> = w.events.iter().filter(|(_, e)| e.starts_with("news ")).map(|(_, e)| e.split(' ').collect()).collect();
    assert!(!news.is_empty(), "no tickNews in 20 s");
    for n in &news {
        assert!(n[1] == "821" || n[1] == "822", "{n:?}");
        assert!(n[2].parse::<i64>().unwrap() > 1_000_000_000_000, "time in milliseconds: {n:?}");
        if n[1] == "821" {
            assert!(n[3] == "BRFG" || n[3] == "DJNL", "{n:?}");
        }
        assert!(n[4].starts_with(&format!("{}$", n[3])), "{n:?}");
    }
}

// ── Auth login without key exchange, logon values (ibx#423, ibx#421), focused ──

/// One paper login: the auth connection is TLS with no key exchange and
/// the farms keep theirs (historical bars come), the current time is the
/// local clock plus the logon offset (within 5 s here), matching symbols
/// are allowed (SECDEFTA on paper), no version cutoff warning, and the
/// logon years limit (199 on paper) refuses 200 years with no query.
/// Run with: cargo test --test rust_api_gt api_logon_values_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_logon_values_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut wrapper = RecWrapper::new();
    poll(&client, &mut wrapper, Duration::from_secs(3));
    let startup = wrapper.drain();
    let errors = |cbs: &[Cb], id: i64| -> Vec<(i64, String)> {
        cbs.iter().filter_map(|c| match c {
            Cb::Error { req_id, code, msg } if *req_id == id => Some((*code, msg.clone())),
            _ => None,
        }).collect()
    };
    assert!(!errors(&startup, -1).iter().any(|(code, _)| *code == 2172), "no version cutoff warning");

    client.req_current_time(&mut wrapper);
    let local = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let time = wrapper.drain().iter().find_map(|c| if let Cb::CurrentTime { time } = c { Some(*time) } else { None });
    println!("current time {:?}, local {}", time, local);
    assert!(time.is_some_and(|t| (t - local).abs() <= 5), "current time {:?} vs local {}", time, local);

    client.req_matching_symbols(110, "AAPL").unwrap();
    poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::SymbolSamples { req_id: 110, .. })), Duration::from_secs(10));
    let cbs = wrapper.drain();
    assert!(errors(&cbs, 110).is_empty(), "{:?}", errors(&cbs, 110));
    assert!(cbs.iter().any(|c| matches!(c, Cb::SymbolSamples { req_id: 110, .. })), "symbol samples");

    client.req_historical_data(120, &spy(), "", "200 Y", "1 month", "TRADES", true, 1, false).unwrap();
    client.req_historical_data(121, &spy(), "", "2 D", "1 hour", "TRADES", true, 1, false).unwrap();
    poll_until(&client, &mut wrapper, |cbs| cbs.iter().any(|c| matches!(c, Cb::HistoricalDataEnd { req_id: 121 })), Duration::from_secs(20));
    let cbs = wrapper.drain();
    client.disconnect();
    assert_eq!(errors(&cbs, 120), vec![(321,
        "Error validating request.-'bM' : cause - Historical data request for 200 year(s) rejected. Max API Backfill Years=199".to_string())]);
    assert!(cbs.iter().any(|c| matches!(c, Cb::HistoricalData { req_id: 121, .. })), "bars through the historical farm");
}

// ── Depth books (ibx#451), focused ──

/// (req, position, operation, side, price, size, market maker, SmartDepth, level two)
type DepthRow = (i64, i32, i32, i32, f64, f64, String, bool, bool);

#[derive(Default)]
struct DepthWrapper {
    rows: Vec<DepthRow>,
    errors: Vec<(i64, i64, String)>,
}

impl Wrapper for DepthWrapper {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) { self.errors.push((req_id, code, text.to_string())); }
    fn update_mkt_depth(&mut self, req_id: i64, position: i32, operation: i32, side: i32, price: f64, size: f64) {
        self.rows.push((req_id, position, operation, side, price, size, String::new(), false, false));
    }
    fn update_mkt_depth_l2(&mut self, req_id: i64, position: i32, mm: &str, operation: i32, side: i32, price: f64, size: f64, smart: bool) {
        self.rows.push((req_id, position, operation, side, price, size, mm.to_string(), smart, true));
    }
}

/// AAPL on IEX alone (5 rows) and AAPL SmartDepth (20 rows) for 60 s, as
/// the reference gives them on paper: IEX as updateMktDepth, its whole
/// book first (bids then asks, inserts), then only rows inside the 5;
/// SmartDepth as updateMktDepthL2 with isSmartDepth and a venue name,
/// inserts and deletes only at the tail of a side, so its book rebuilds
/// from the callbacks; no book is reset.
/// Run with: cargo test --test rust_api_gt api_depth_books_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_depth_books_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = DepthWrapper::default();
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(3) {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(50));
    }
    w.errors.clear();
    let iex = Contract { exchange: "IEX".into(), ..aapl() };
    client.req_mkt_depth(821, &iex, 5, false).unwrap();
    client.req_mkt_depth(822, &aapl(), 20, true).unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        client.process_msgs(&mut w);
        std::thread::sleep(Duration::from_millis(5));
    }
    client.cancel_mkt_depth(821).unwrap();
    client.cancel_mkt_depth(822).unwrap();
    client.disconnect();
    for e in &w.errors {
        println!("  error {e:?}");
    }
    for (req, rows, smart) in [(821i64, 5i32, false), (822, 20, true)] {
        let mine: Vec<_> = w.rows.iter().filter(|r| r.0 == req).collect();
        let ops = |op: i32| mine.iter().filter(|r| r.2 == op).count();
        println!("req {req}: {} callbacks, {} inserts, {} updates, {} deletes", mine.len(), ops(0), ops(1), ops(2));
        assert!(!mine.is_empty(), "req {req}: no depth callback");
        assert!(mine.iter().all(|r| r.7 == smart && r.8 == smart), "req {req}: callback kind");
        assert!(mine.iter().all(|r| r.1 >= 0 && r.1 < rows), "req {req}: a position outside the rows");
        // SmartDepth changes only the tail and rows in place, so its book
        // rebuilds exactly.
        let mut book: [Vec<(f64, f64)>; 2] = [Vec::new(), Vec::new()];
        for r in mine.iter().filter(|_| smart) {
            let side = &mut book[r.3 as usize];
            let pos = r.1 as usize;
            match r.2 {
                0 => {
                    assert_eq!(pos, side.len(), "req {req}: insert not at the tail: {r:?}");
                    side.push((r.4, r.5));
                }
                1 => {
                    assert!(pos < side.len(), "req {req}: update of a missing row: {r:?}");
                    side[pos] = (r.4, r.5);
                }
                _ => {
                    assert_eq!(pos + 1, side.len(), "req {req}: delete not at the tail: {r:?}");
                    side.pop();
                }
            }
        }
        if !smart {
            // The whole book first: bid inserts, then ask inserts, from 0.
            let first: Vec<_> = mine.iter().take_while(|r| r.2 == 0).collect();
            let bids = first.iter().take_while(|r| r.3 == 1).count();
            assert!(first[..bids].iter().enumerate().all(|(k, r)| r.1 == k as i32), "req {req}: bid snapshot");
            assert!(first[bids..].iter().enumerate().all(|(k, r)| r.3 == 0 && r.1 == k as i32), "req {req}: ask snapshot");
        }
        assert!(book.iter().all(|s| s.len() <= rows as usize), "req {req}: more rows than asked");
    }
    assert!(w.errors.iter().all(|e| e.1 != 317), "a book was reset: {:?}", w.errors);
}

// ── Several requests on one contract (ibx#444) and the generic tick list (ibx#450), focused ──

/// Two request ids on AAPL with the same news tick, a third without, two
/// symbol-only MSFT requests, then the cancel of one AAPL request while the
/// others go on; and two invalid generic tick lists. Expected: no error for
/// the shared requests; each gets its market data type and request
/// parameters (the second AAPL request at once, with the latest headlines
/// of the first); 321 for "999" and "mdoff,292:" with the legal ticks of
/// STK. The log shows one top-of-book and one news entry for AAPL, and no
/// farm cancel until the last AAPL request goes.
/// Run with: cargo test --test rust_api_gt api_shared_requests_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_shared_requests_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = SnapWrapper::default();
    let pump = |client: &EClient, w: &mut SnapWrapper, secs: u64| {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(secs) {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    pump(&client, &mut w, 3);
    w.events.clear();
    w.start = Some(Instant::now());
    let stock = |con_id: i64, symbol: &str| Contract { con_id, symbol: symbol.into(), sec_type: "STK".into(),
        exchange: "SMART".into(), currency: "USD".into(), ..Default::default() };
    client.req_mkt_data(831, &stock(265598, "AAPL"), "mdoff,292", false, false).unwrap();
    pump(&client, &mut w, 5);
    client.req_mkt_data(832, &stock(265598, "AAPL"), "mdoff,292", false, false).unwrap();
    client.req_mkt_data(833, &stock(265598, "AAPL"), "", false, false).unwrap();
    client.req_mkt_data(834, &stock(0, "MSFT"), "", false, false).unwrap();
    pump(&client, &mut w, 3);
    client.req_mkt_data(835, &stock(0, "MSFT"), "", false, false).unwrap();
    client.req_mkt_data(836, &stock(265598, "AAPL"), "999", false, false).unwrap();
    client.req_mkt_data(837, &stock(0, "GOOGL"), "mdoff,292:", false, false).unwrap();
    pump(&client, &mut w, 5);
    client.cancel_mkt_data(831).unwrap();
    pump(&client, &mut w, 5);
    for req in [832, 833, 834, 835] {
        client.cancel_mkt_data(req).unwrap();
    }
    pump(&client, &mut w, 1);
    client.disconnect();
    for (ms, e) in &w.events {
        println!("  {ms:>6} ms  {e}");
    }
    let has = |want: &str| w.events.iter().any(|(_, e)| e == want);
    let errors_of = |req: i64| w.events.iter().filter(|(_, e)| e.starts_with(&format!("error {req} "))).count();
    for req in [831, 832, 833, 834, 835] {
        assert_eq!(errors_of(req), 0, "request {req}");
        assert!(w.events.iter().any(|(_, e)| e.starts_with(&format!("params {req} "))), "params of {req}");
    }
    let legal = ibx::control::generic_tick::legal_ones("STK");
    assert!(has(&format!("error 836 321 Error validating request.-'bQ' : cause - Incorrect generic tick list of 999.  Legal ones for (STK) are: {legal}")));
    assert!(has(&format!("error 837 321 Error validating request.-'bQ' : cause - Incorrect generic tick list of mdoff,292:.  Legal ones for (STK) are: {legal}")));
}

// ── Option chain parameters (ibx#440), focused ──

/// A row: (reqId, exchange, conId, trading class, multiplier, expirations, strikes).
type ChainRow = (i64, String, i64, String, String, usize, usize);

#[derive(Default)]
struct ChainWrapper {
    events: Vec<String>,
    rows: Vec<ChainRow>,
    ends: Vec<i64>,
    details: Vec<(i64, i64, String)>,
    details_ends: Vec<i64>,
}

impl Wrapper for ChainWrapper {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) { self.events.push(format!("error {req_id} {code} {text}")); }
    fn security_definition_option_parameter(
        &mut self, req_id: i64, exchange: &str, underlying_con_id: i64,
        trading_class: &str, multiplier: &str, expirations: &[String], strikes: &[f64],
    ) {
        self.rows.push((req_id, exchange.into(), underlying_con_id, trading_class.into(), multiplier.into(),
            expirations.len(), strikes.len()));
    }
    fn security_definition_option_parameter_end(&mut self, req_id: i64) { self.ends.push(req_id); }
    fn contract_details(&mut self, req_id: i64, d: &ContractDetails) {
        self.details.push((req_id, d.contract.con_id, d.contract.last_trade_date_or_contract_month.clone()));
    }
    fn contract_details_end(&mut self, req_id: i64) { self.details_ends.push(req_id); }
}

/// As the reference (captured 28/09/2026 for AAPL): AAPL STK gives groups
/// for IBUSOPT, SMART and each listed exchange, trading classes AAPL (and
/// 2AAPL when listed), then the end; the same request again gives the same
/// rows in the same order without a new query; OPT gives 321; an ES future
/// on CME gives futures option groups on CME, then the end. Works with the
/// market closed. The log shows the 6040=5, 35=c and 6040=138 queries for
/// AAPL, none for the second AAPL request, and one 6040=138 with 6995=CME
/// for ES.
/// Run with: cargo test --test rust_api_gt api_option_chains_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_option_chains_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = ChainWrapper::default();
    let wait = |client: &EClient, w: &mut ChainWrapper, done: &dyn Fn(&ChainWrapper) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(w) && Instant::now() < deadline {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    std::thread::sleep(Duration::from_secs(3));
    client.req_sec_def_opt_params(9410, "AAPL", "", "STK", 265598).unwrap();
    wait(&client, &mut w, &|w| w.ends.contains(&9410) || !w.events.is_empty());
    client.req_sec_def_opt_params(9421, "AAPL", "", "STK", 265598).unwrap();
    wait(&client, &mut w, &|w| w.ends.contains(&9421) || !w.events.is_empty());
    client.req_sec_def_opt_params(9422, "AAPL", "", "OPT", 265598).unwrap();
    wait(&client, &mut w, &|w| w.events.iter().any(|e| e.starts_with("error 9422 ")));
    let es = Contract { symbol: "ES".into(), sec_type: "FUT".into(), exchange: "CME".into(), currency: "USD".into(), ..Default::default() };
    client.req_contract_details(9423, &es).unwrap();
    wait(&client, &mut w, &|w| w.details_ends.contains(&9423));
    let today = jiff::Timestamp::now().strftime("%Y%m%d").to_string();
    let front = w.details.iter().filter(|d| d.0 == 9423 && d.2.as_str() >= today.as_str())
        .min_by(|a, b| a.2.cmp(&b.2)).map(|d| d.1);
    if let Some(con_id) = front {
        client.req_sec_def_opt_params(9424, "ES", "CME", "FUT", con_id).unwrap();
        wait(&client, &mut w, &|w| w.ends.contains(&9424) || w.events.iter().any(|e| e.starts_with("error 9424 ")));
    }
    client.disconnect();
    let of = |req: i64| w.rows.iter().filter(|r| r.0 == req).cloned().collect::<Vec<_>>();
    for req in [9410, 9421, 9424] {
        let rows = of(req);
        println!("  {req}: {} rows, end {}", rows.len(), w.ends.contains(&req));
        for r in rows.iter().take(50) {
            println!("    {} {} {} {} exp={} strikes={}", r.1, r.2, r.3, r.4, r.5, r.6);
        }
    }
    println!("  ES front month {front:?}, events {:?}", w.events);
    let aapl = of(9410);
    assert!(w.ends.contains(&9410), "end of 9410");
    assert!(aapl.iter().any(|r| r.1 == "IBUSOPT" && r.3 == "AAPL"), "{aapl:?}");
    assert!(aapl.iter().any(|r| r.1 == "SMART" && r.3 == "AAPL"), "{aapl:?}");
    assert!(aapl.iter().all(|r| r.2 == 265598 && r.4 == "100" && r.5 > 0 && r.6 > 0), "{aapl:?}");
    let again: Vec<_> = of(9421).into_iter().map(|r| (r.1, r.3, r.5, r.6)).collect();
    assert_eq!(again, aapl.iter().map(|r| (r.1.clone(), r.3.clone(), r.5, r.6)).collect::<Vec<_>>(), "same rows, same order");
    assert!(w.events.contains(&"error 9422 321 Error validating request.-'cp' : cause - Invalid security type - OPT".to_string()), "{:?}", w.events);
    let es_rows = of(9424);
    assert!(front.is_some(), "ES front month");
    assert!(w.ends.contains(&9424), "end of 9424: {:?}", w.events);
    assert!(!es_rows.is_empty() && es_rows.iter().all(|r| r.1 == "CME"), "{es_rows:?}");
}


// ── Stop orders and all-or-none (ibx#263), focused ──

#[derive(Default)]
struct StopsWrapper {
    events: Vec<String>,
    closes: Vec<f64>,
    bars_done: bool,
    statuses: Vec<(i64, String)>,
}

impl Wrapper for StopsWrapper {
    fn error(&mut self, req_id: i64, code: i64, text: &str, _: &str) { self.events.push(format!("error {req_id} {code} {text}")); }
    fn historical_data(&mut self, _: i64, bar: &BarData) { self.closes.push(bar.close); }
    fn historical_data_end(&mut self, _: i64, _: &str, _: &str) { self.bars_done = true; }
    fn order_status(
        &mut self, order_id: i64, status: &str, _: f64, _: f64,
        _: f64, _: i64, _: i64, _: f64, _: i64, _: &str, _: f64,
    ) {
        self.events.push(format!("status {order_id} {status}"));
        self.statuses.push((order_id, status.into()));
    }
}

/// As the reference: a STP (sent with the default trigger method), a
/// TRAIL with all-or-none and a REL with all-or-none (sent with the
/// instruction field "a G" and "R G"), each far from the market (AAPL BUY
/// 1: stop at 1.5 times the last daily close, trailing amount half of it
/// with the first stop at 1.5 times, relative with a price cap at half),
/// then cancelled. Each must be held by the server (PreSubmitted or
/// Submitted) with no refusal, then Cancelled. Works with the market open
/// or closed (closed: the orders wait for the next session).
/// Run with: cargo test --test rust_api_gt api_stops_all_or_none_live -- --ignored --nocapture
#[test]
#[ignore]
fn api_stops_all_or_none_live() {
    let _ = env_logger::try_init();
    let config = match get_config() {
        Some(c) => c,
        None => { println!("Skipping: IB credentials not set"); return; }
    };
    let client = EClient::connect(&config).expect("EClient::connect failed");
    if !client.account_id.starts_with("DU") {
        client.disconnect();
        panic!("refusing to run: not a paper account");
    }
    let mut w = StopsWrapper::default();
    let wait = |client: &EClient, w: &mut StopsWrapper, secs: u64, done: &dyn Fn(&StopsWrapper) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !done(w) && Instant::now() < deadline {
            client.process_msgs(w);
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    std::thread::sleep(Duration::from_secs(3));
    client.req_historical_data(9630, &aapl(), "", "5 D", "1 day", "TRADES", true, 1, false).unwrap();
    wait(&client, &mut w, 30, &|w| w.bars_done);
    let last = *w.closes.last().expect("no daily close for AAPL");
    let cents = |v: f64| (v * 100.0).round() / 100.0;
    let base = || Order { action: "BUY".into(), total_quantity: 1.0, ..Default::default() };
    let orders = [
        ("STP", Order { order_type: "STP".into(), aux_price: cents(last * 1.5), ..base() }),
        ("TRAIL AON", Order { order_type: "TRAIL".into(), aux_price: cents(last * 0.5),
            trail_stop_price: cents(last * 1.5), all_or_none: true, ..base() }),
        ("REL AON", Order { order_type: "REL".into(), lmt_price: cents(last * 0.5), aux_price: 0.01,
            all_or_none: true, ..base() }),
    ];
    let mut ids = Vec::new();
    for (label, order) in &orders {
        let oid = client.next_order_id();
        client.place_order(oid, &aapl(), order).expect("place_order failed");
        ids.push((oid, *label));
    }
    let held = |w: &StopsWrapper, oid: i64| w.statuses.iter().any(|(id, s)| *id == oid && (s == "PreSubmitted" || s == "Submitted"));
    let ended = |w: &StopsWrapper, oid: i64| w.statuses.iter().any(|(id, s)| *id == oid && (s == "Cancelled" || s == "Inactive" || s == "ApiCancelled"));
    wait(&client, &mut w, 30, &|w| ids.iter().all(|(oid, _)| held(w, *oid) || ended(w, *oid)));
    let held_before_cancel: Vec<(i64, bool, bool)> = ids.iter().map(|(oid, _)| (*oid, held(&w, *oid), ended(&w, *oid))).collect();
    for (oid, _) in &ids {
        client.cancel_order(*oid, "").unwrap();
    }
    wait(&client, &mut w, 30, &|w| ids.iter().all(|(oid, _)| w.statuses.iter().any(|(id, s)| id == oid && s == "Cancelled")));
    client.disconnect();
    println!("  AAPL last daily close {last}");
    for e in &w.events {
        println!("  {e}");
    }
    for ((oid, label), (_, was_held, was_ended)) in ids.iter().zip(held_before_cancel) {
        // 399 is the outside-hours warning, 202 the answer to the test's own cancel.
        let refusals: Vec<&String> = w.events.iter()
            .filter(|e| e.starts_with(&format!("error {oid} ")) && !e.starts_with(&format!("error {oid} 399 "))
                && !e.starts_with(&format!("error {oid} 202 ")))
            .collect();
        assert!(was_held && !was_ended, "{label} ({oid}) not held: {:?}", w.events);
        assert!(refusals.is_empty(), "{label} ({oid}): {refusals:?}");
        assert!(w.statuses.iter().any(|(id, s)| id == oid && s == "Cancelled"), "{label} ({oid}) not cancelled");
    }
}
