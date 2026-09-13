//! Market data subscription test phases.

use super::common::*;
use ibx::control::contracts;
use ibx::protocol::fix;

pub(super) fn phase_market_data(conns: Conns) -> Conns {
    phase!("--- Phase 2: Market Data Ticks (AAPL) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );

    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id: 265598, symbol: "AAPL".into(), exchange: String::new(), sec_type: String::new(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut tick_count = 0u32;
    let mut first_tick = false;

    while Instant::now() < deadline {
        if let Ok(Event::Tick(instrument)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            tick_count += 1;
            if !first_tick {
                let q = shared.market.quote(instrument);
                let bid = q.bid as f64 / PRICE_SCALE as f64;
                let ask = q.ask as f64 / PRICE_SCALE as f64;
                let last = q.last as f64 / PRICE_SCALE as f64;
                println!(
                    "  FIRST TICK: instrument={instrument} bid={bid:.2} ask={ask:.2} last={last:.2}"
                );
                // Value assertions
                if q.bid > 0 && q.ask > 0 {
                    assert!(bid > 50.0 && bid < 1000.0, "AAPL bid out of range: {bid}");
                    assert!(ask > 50.0 && ask < 1000.0, "AAPL ask out of range: {ask}");
                    assert!(ask >= bid, "Crossed market: bid={bid} ask={ask}");
                }
                first_tick = true;
            }
        }
        if first_tick {
            break;
        }
    }

    if !first_tick {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        no_market(&shared, "no ticks in 30s");
        return conns;
    }

    std::thread::sleep(Duration::from_secs(5));
    tick_count += drain_ticks(&event_rx);

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    println!("  PASS ({tick_count} ticks)\n");
    conns
}

pub(super) fn phase_multi_instrument(conns: Conns) -> Conns {
    phase!("--- Phase 3: Multi-Instrument Subscription (AAPL+MSFT+SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );

    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id: 265598, symbol: "AAPL".into(), exchange: String::new(), sec_type: String::new(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id: 272093, symbol: "MSFT".into(), exchange: String::new(), sec_type: String::new(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id: 756733, symbol: "SPY".into(), exchange: String::new(), sec_type: "STK".into(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut tick_count = 0u32;
    let mut first_tick = false;

    while Instant::now() < deadline {
        if let Ok(Event::Tick(_)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            tick_count += 1;
            if !first_tick {
                first_tick = true;
            }
        }
        if first_tick {
            break;
        }
    }

    if !first_tick {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        no_market(&shared, "no ticks");
        return conns;
    }

    std::thread::sleep(Duration::from_secs(5));
    tick_count += drain_ticks(&event_rx);

    // Check each instrument received distinct prices
    let mut instruments_with_data = 0u32;
    for id in 0..3u32 {
        let q = shared.market.quote(id);
        if q.bid > 0 || q.ask > 0 || q.last > 0 {
            instruments_with_data += 1;
            let bid = q.bid as f64 / PRICE_SCALE as f64;
            let ask = q.ask as f64 / PRICE_SCALE as f64;
            println!("  Instrument {id}: bid={bid:.2} ask={ask:.2}");
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    // Ticks arriving without a quote on them is the venue saying no quote
    // exists, which is the same thing as no ticks at all and is read the same
    // way: outside regular hours it is the market, inside them it is this
    // client. Counted as ticks and no further, a session where the venue
    // printed for one name and had no quote for the others read as two
    // instruments gone missing.
    if tick_count <= 3 || instruments_with_data < 2 {
        no_market(&shared, &format!(
            "{tick_count} ticks and {instruments_with_data} of 3 instruments quoted, \
             too few for a multi-instrument test",
        ));
    } else {
        println!(
            "  PASS ({tick_count} ticks, {instruments_with_data} instruments with data)\n"
        );
    }
    conns
}

pub(super) fn phase_subscribe_unsubscribe(conns: Conns) -> Conns {
    phase!("--- Phase 16: Subscribe + Unsubscribe Cleanup ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );
    let (registered_tx, registered_rx) = std::sync::mpsc::sync_channel(1);
    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id: 756733, symbol: "SPY".into(), exchange: String::new(), sec_type: "STK".into(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: Some(registered_tx),
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    let join = run_hot_loop(hot_loop);

    // Which slot the engine gave this contract, so the withdrawal below names
    // the subscription that was actually opened. Assuming the first slot makes
    // the silence that follows prove nothing: an unsubscribe aimed at a slot
    // nobody holds is quiet for the same reason a working one is.
    let instrument = registered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the engine never answered the subscription")
        .expect("the subscription was refused");

    // The feed has to be waited for rather than sampled. A fixed window opened
    // at the send is spent on starting the loop, resolving the contract and
    // the round trip to the venue, so what it measures is whatever is left of
    // it — on a liquid name that is the difference between one tick and none.
    let mut tick_count = 0u32;
    let first_tick_by = Instant::now() + Duration::from_secs(30);
    while Instant::now() < first_tick_by && tick_count == 0 {
        if let Ok(Event::Tick(_)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            tick_count += 1;
        }
    }
    if tick_count > 0 {
        let observe = Instant::now() + Duration::from_secs(2);
        while Instant::now() < observe {
            if let Ok(Event::Tick(_)) = event_rx.recv_timeout(Duration::from_millis(100)) {
                tick_count += 1;
            }
        }
    }

    control_tx
        .send(ControlCommand::Unsubscribe { instrument, con_id: 0, took_it: 0, series: Vec::new(), issued: 0 })
        .unwrap();
    // Ticks already in flight when the withdrawal is sent are absorbed rather
    // than counted.
    let settle = Instant::now() + Duration::from_secs(2);
    while Instant::now() < settle {
        let _ = event_rx.recv_timeout(Duration::from_millis(100));
    }
    // Then silence is required. Without a read after the withdrawal, a
    // subscription that is never withdrawn, and one that never starts, both pass.
    let mut after_unsubscribe = 0u32;
    let quiet = Instant::now() + Duration::from_secs(3);
    while Instant::now() < quiet {
        if let Ok(Event::Tick(_)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            after_unsubscribe += 1;
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    println!("  Total ticks: {tick_count}, after the withdrawal: {after_unsubscribe}");
    if tick_count == 0 {
        no_market(&shared, "no ticks arrived while subscribed");
        return conns;
    }
    assert_eq!(
        after_unsubscribe, 0,
        "ticks were still arriving three seconds after the subscription was withdrawn",
    );
    println!("  PASS ({tick_count} ticks, then silence)\n");
    conns
}

pub(super) fn phase_market_depth(conns: Conns) -> Conns {
    phase!("--- Phase 130: Market Depth Subscribe/Unsubscribe (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, _event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );
    let join = run_hot_loop(hot_loop);

    let req_id = 93001u32;
    control_tx
        .send(ControlCommand::SubscribeDepth {
            contract: ContractRef { con_id: 756733, exchange: "SMART".into(), sec_type: "STK".into(), symbol: "".to_string(), currency: "".to_string(), ..Default::default() },
            req_id,
            num_rows: 5,
            is_smart_depth: true,
            filters: Default::default(),
        })
        .unwrap();

    // Wait for the book rather than for the clock. A flat four second sleep
    // reported no depth on a book that does deliver, because the first rows
    // had not arrived yet.
    let mut depth_updates = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        depth_updates.extend(shared.market.drain_depth_updates());
        if !depth_updates.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    control_tx
        .send(ControlCommand::UnsubscribeDepth { req_id })
        .unwrap();

    // Aggregated depth is entitled separately from a single exchange book, so a
    // silent aggregated subscription is retried directly before concluding
    // anything about the client.
    if depth_updates.is_empty() {
        // The exchange the contract trades on, rather than the smart
        // destination again. The venue serves no book there for a share — it
        // lists the top of one and nothing deeper, in every region — so asking
        // twice in the same place asks twice for something that does not
        // exist. Asked for on six exchanges, the venue refused five and took
        // this one, which is the one this account holds depth for.
        let direct_id = req_id + 1;
        control_tx
            .send(ControlCommand::SubscribeDepth {
                contract: ContractRef { con_id: 756733, exchange: "IEX".into(), sec_type: "STK".into(), symbol: "".to_string(), currency: "".to_string(), ..Default::default() },
                req_id: direct_id,
                num_rows: 5,
                is_smart_depth: false,
                filters: Default::default(),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            depth_updates.extend(shared.market.drain_depth_updates());
            if !depth_updates.is_empty() {
                println!("  (aggregated depth was silent, the exchange book delivers)");
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        control_tx
            .send(ControlCommand::UnsubscribeDepth { req_id: direct_id })
            .unwrap();
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    if depth_updates.is_empty() {
        // A book this account is not entitled to is refused by the venue, in
        // words and under the number the reference client reports it with. A
        // phase that read that as a quiet market said nothing about either.
        let refused = shared.reference.take_error_for(req_id + 1)
            .or_else(|| shared.reference.take_error_for(req_id));
        match refused {
            Some((code, why)) => {
                skipped!("  SKIP: the venue will not serve this book: {why} ({code})\n");
            }
            None => no_market(&shared, "no depth updates and no refusal either"),
        }
    } else {
        println!("  PASS ({} depth updates)\n", depth_updates.len());
    }
    conns
}

pub(super) fn phase_news_ticks(conns: Conns) -> Conns {
    phase!("--- Phase 131: News Tick Subscribe/Unsubscribe (AAPL) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );
    let join = run_hot_loop(hot_loop);

    control_tx
        .send(ControlCommand::SubscribeNews {
            con_id: 265598,
            symbol: "AAPL".into(),
            sec_type: "STK".into(),
            providers: "BZ+FLY".into(),
            reply_tx: None,
        })
        .unwrap();

    let mut news_events = 0u32;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(Event::News(_)) = event_rx.recv_timeout(Duration::from_millis(150)) { news_events += 1 }
    }

    control_tx
        .send(ControlCommand::UnsubscribeNews { subject: ibx::types::NewsSubject::Slot(0) })
        .unwrap();
    let drained_news = shared.market.drain_tick_news();

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    if news_events == 0 && drained_news.is_empty() {
        skipped!("  SKIP: No tick news in 8s (normal if no live headlines)\n");
    } else {
        println!(
            "  PASS ({} event news, {} drained headlines)\n",
            news_events,
            drained_news.len()
        );
    }
    conns
}

pub(super) fn phase_tbt_subscribe(conns: Conns) -> Conns {
    phase!("--- Phase 61: Tick-by-Tick Data (SPY via HMDS) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );
    control_tx
        .send(ControlCommand::SubscribeTbt {
            contract: ContractRef { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".to_string(), exchange: "SMART".to_string(), ..Default::default() },
            req_id: 1,
            number_of_ticks: 0,
            ignore_size: false,
            tbt_type: TbtType::Last,
            reply_tx: None,
        })
        .unwrap();
    let join = run_hot_loop(hot_loop);

    let mut first_tbt = false;
    let mut tbt_trade_count = 0u32;
    let mut tbt_quote_count = 0u32;
    let deadline = Instant::now() + Duration::from_secs(30);

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::TbtTrade(trade)) => {
                if tbt_trade_count == 0 {
                    println!(
                        "  First TBT trade: price={} size={} exchange={}",
                        trade.price as f64 / PRICE_SCALE as f64,
                        trade.size,
                        trade.exchange
                    );
                    first_tbt = true;
                }
                tbt_trade_count += 1;
                break;
            }
            Ok(Event::TbtQuote(quote)) => {
                if tbt_quote_count == 0 {
                    println!(
                        "  First TBT quote: bid={} ask={}",
                        quote.bid as f64 / PRICE_SCALE as f64,
                        quote.ask as f64 / PRICE_SCALE as f64
                    );
                    first_tbt = true;
                }
                tbt_quote_count += 1;
                break;
            }
            _ => {}
        }
    }

    if !first_tbt {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        no_market(&shared, "no tick-by-tick data in 30s");
        return conns;
    }

    std::thread::sleep(Duration::from_secs(5));
    let conns = shutdown_and_reclaim(&control_tx, join, account_id);
    println!(
        "  PASS ({tbt_trade_count} trades, {tbt_quote_count} quotes)\n"
    );
    conns
}

pub(super) fn phase_streaming_validation(conns: Conns) -> Conns {
    phase!("--- Phase 102: Streaming Data Validation (SPY tick quality) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );

    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id: 756733, symbol: "SPY".into(), exchange: String::new(), sec_type: "STK".into(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut tick_count = 0u32;
    let mut bid_positive = false;
    let mut ask_positive = false;
    let mut spread_valid = true;
    let mut price_reasonable = true;

    while Instant::now() < deadline {
        if let Ok(Event::Tick(instrument)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            tick_count += 1;
            let q = shared.market.quote(instrument);
            let bid = q.bid as f64 / PRICE_SCALE as f64;
            let ask = q.ask as f64 / PRICE_SCALE as f64;

            if q.bid > 0 {
                bid_positive = true;
            }
            if q.ask > 0 {
                ask_positive = true;
            }

            // Validate spread: ask >= bid (when both are set)
            // Tolerate up to 5 cents of momentary crossing — bid/ask ticks
            // arrive in separate messages so the SeqLock snapshot can show
            // a transient cross until the next tick updates the other side.
            let cross_tolerance = 0.05 * PRICE_SCALE as f64; // 5 cents
            if q.bid > 0 && q.ask > 0 && (q.bid - q.ask) as f64 > cross_tolerance {
                spread_valid = false;
                println!("  WARNING: Crossed market bid={bid:.4} ask={ask:.4}");
            }

            // SPY should be between $50 and $1000
            if q.bid > 0 && !(50.0..=1000.0).contains(&bid) {
                price_reasonable = false;
                println!("  WARNING: Bid out of range: {bid:.4}");
            }
            if q.ask > 0 && !(50.0..=1000.0).contains(&ask) {
                price_reasonable = false;
                println!("  WARNING: Ask out of range: {ask:.4}");
            }

            if tick_count >= 20 {
                break;
            }
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if tick_count == 0 {
        no_market(&shared, "no ticks arrived");
        return conns;
    }

    println!(
        "  {tick_count} ticks: bid_positive={bid_positive} ask_positive={ask_positive} spread_valid={spread_valid} price_reasonable={price_reasonable}"
    );
    // A tick carrying no quote on either side is the venue saying there is
    // none, which outside trading hours is every tick it sends: the close and
    // the day's volume arrive, the book does not. The same answer as silence,
    // and `no_market` still fails outright if the clock says regular hours.
    if !bid_positive && !ask_positive {
        no_market(&shared, "the venue states no quote on either side");
        return conns;
    }
    assert!(bid_positive, "Should have seen at least one positive bid");
    assert!(ask_positive, "Should have seen at least one positive ask");
    assert!(spread_valid, "Spread should not be crossed (ask >= bid)");
    assert!(
        price_reasonable,
        "Prices should be in reasonable range for SPY"
    );
    println!("  PASS\n");
    conns
}

pub(super) fn phase_fallback_market_data(conns: Conns) -> Conns {
    let (symbol, sec_type, exchange) = ALWAYS_QUOTING;
    phase!("--- Phase 107: Fallback Market Data Ticks ({symbol} on {exchange} — session-independent) ---");

    // Look up the contract first
    let now = ibx::protocol::datetime::chrono_free_timestamp();
    let mut ccp = conns.ccp;
    ccp.send_fix(&[
        (fix::TAG_MSG_TYPE, "c"),
        (fix::TAG_SENDING_TIME, &now),
        (contracts::TAG_SECURITY_REQ_ID, "RFX107"),
        (contracts::TAG_SECURITY_REQ_TYPE, "2"),
        (contracts::TAG_SYMBOL, symbol),
        (contracts::TAG_SECURITY_TYPE, sec_type),
        (contracts::TAG_EXCHANGE, exchange),
        (contracts::TAG_CURRENCY, "USD"),
        (contracts::TAG_IB_SOURCE, "Socket"),
    ])
    .expect("Failed to send forex secdef request");

    let mut forex_con_id: Option<i64> = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && forex_con_id.is_none() {
        match ccp.try_recv() {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(_) => break,
            Ok(_) => {}
        }
        for frame in ccp.extract_frames() {
            // Every frame is unsigned, whatever kind it is. On a signed session a
            // frame read as it stands parses distorted, and unsigning is what
            // advances the read chain, so skipping one leaves the rest unreadable.
            for msg in messages_in(&mut ccp, &frame) {
                let tags = fix::fix_parse(&msg);
                if tags.get(&fix::TAG_MSG_TYPE).map(|s| s.as_str()) == Some("d")
                    && let Some(def) = contracts::parse_secdef_response(&msg, true)
                        // The one that was asked for. Any definition with an
                        // id passes for it otherwise, and a reply still in
                        // flight from an earlier phase overwrites the answer
                        // with a contract these phases never subscribe to.
                        && def.symbol == ALWAYS_QUOTING.0
                        && def.con_id > 0 {
                            forex_con_id = Some(def.con_id as i64);
                        }
            }
        }
    }

    let con_id = match forex_con_id {
        Some(id) => id,
        None => {
            lookup_returned_nothing(&format!("no {symbol} contract came back"));
        }
    };
    // Read by the two phases after this one.
    let _ = FALLBACK_CON_ID.set(con_id);

    // Subscribe and verify ticks
    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        ccp,
        conns.hmds,
        None,
    );

    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id, symbol: ALWAYS_QUOTING.0.into(),
                // Named, not left to the default. A subscription with no
                // exchange is asked for on BEST, and the venue refuses a
                // crypto there: "BEST/CRYPTO/Top". The ticks still arrive and
                // carry no prices, so the phase saw a stream and no quote.
                exchange: ALWAYS_QUOTING.2.into(), sec_type: ALWAYS_QUOTING.1.into(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut tick_count = 0u32;
    // One tick carrying both sides at once, not a bid on one tick and an ask
    // on another: a book that never had two sides together is not a quote,
    // and the spread below is only a spread when it came from one.
    let mut two_sided = false;

    while Instant::now() < deadline {
        if let Ok(Event::Tick(instrument)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            tick_count += 1;
            let q = shared.market.quote(instrument);
            if q.bid > 0 && q.ask > 0 {
                two_sided = true;
            }

            if tick_count == 1 {
                println!(
                    "  FIRST TICK: bid={:.5} ask={:.5}",
                    q.bid as f64 / PRICE_SCALE as f64,
                    q.ask as f64 / PRICE_SCALE as f64
                );
            }

            // Only where this tick carried both sides. Early ticks arrive with
            // one side still at zero, and comparing against that reads as a
            // crossed market every time.
            if q.bid > 0 && q.ask > 0 {
                assert!(q.ask >= q.bid, "Crossed market: ask < bid");
            }

            if tick_count >= 10 {
                break;
            }
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    // Asserted after the session is reclaimed, so a phase that fails closes
    // what it opened rather than leaving the account's one session held.
    assert!(
        tick_count > 0,
        "{} on {} quotes every day, so no tick at all is this client failing to read one",
        ALWAYS_QUOTING.0, ALWAYS_QUOTING.2
    );
    assert!(
        two_sided,
        "not one of {tick_count} ticks carried a price on both sides at once, \
         so nothing here read a quote"
    );
    println!("  {tick_count} ticks received, a two-sided quote among them");
    println!("  PASS\n");
    conns
}

pub(super) fn phase_fallback_streaming_validation(conns: Conns) -> Conns {
    phase!("--- Phase 108: Fallback Streaming Validation ({} — session-independent) ---", ALWAYS_QUOTING.0);

    // As the venue named it, in the phase before this one. Written in here, a
    // contract id that had changed or that this account sees differently read
    // as a subscription that produced nothing.
    let Some(&con_id) = FALLBACK_CON_ID.get() else {
        skipped!("  SKIP: the phase that asks the venue for {} did not name it\n", ALWAYS_QUOTING.0);
        return conns;
    };

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );

    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id, symbol: ALWAYS_QUOTING.0.into(),
                // Named, not left to the default. A subscription with no
                // exchange is asked for on BEST, and the venue refuses a
                // crypto there: "BEST/CRYPTO/Top". The ticks still arrive and
                // carry no prices, so the phase saw a stream and no quote.
                exchange: ALWAYS_QUOTING.2.into(), sec_type: ALWAYS_QUOTING.1.into(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut tick_count = 0u32;
    // Counted apart from the ticks. A subscription the venue refused still
    // delivers ticks that carry no price, and a spread read off those is a
    // spread this phase never saw.
    let mut quoted = 0u32;
    // How many of those quotes differed. A tick carries no price of its own —
    // the book is read from the cache beside it — so counting ticks alone says
    // a stream is alive without saying the book under it moved. Reported, not
    // asserted: a book can legitimately stand still for the length of a phase.
    let mut distinct: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
    let mut spread_valid = true;

    while Instant::now() < deadline {
        if let Ok(Event::Tick(instrument)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            tick_count += 1;
            let q = shared.market.quote(instrument);
            let bid = q.bid as f64 / PRICE_SCALE as f64;
            let ask = q.ask as f64 / PRICE_SCALE as f64;

            if q.bid > 0 && q.ask > 0 {
                quoted += 1;
                distinct.insert((q.bid, q.ask));
                if q.ask < q.bid {
                    spread_valid = false;
                    println!("  WARNING: Crossed spread bid={bid:.5} ask={ask:.5}");
                }
            }

            if quoted >= 15 {
                break;
            }
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    assert!(
        quoted > 0,
        "{tick_count} ticks arrived and not one carried a price on both sides, \
         so the spread this phase validates was never read"
    );
    assert!(spread_valid, "Spread should not be crossed");
    println!(
        "  {quoted} quoted of {tick_count} ticks, {} of them distinct, spread_valid={spread_valid}",
        distinct.len(),
    );
    println!("  PASS\n");
    conns
}

/// The first tick carrying a price on both sides, or nothing within `within`.
///
/// A subscription the venue refused still delivers ticks — trading status and
/// the like — so a phase that stops at the first `Event::Tick` has shown the
/// stream is alive and nothing at all about the quote it came for.
fn quoted_tick(
    rx: &std::sync::mpsc::Receiver<Event>, shared: &SharedState, within: Duration,
) -> Option<(f64, f64)> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(Event::Tick(instrument)) = rx.recv_timeout(Duration::from_millis(100)) {
            let q = shared.market.quote(instrument);
            if q.bid > 0 && q.ask > 0 {
                return Some((
                    q.bid as f64 / PRICE_SCALE as f64,
                    q.ask as f64 / PRICE_SCALE as f64,
                ));
            }
        }
    }
    None
}

pub(super) fn phase_fallback_resubscribe(conns: Conns) -> Conns {
    // A new hot loop over the transports already open, not a reconnection of
    // the transports themselves: what this shows is that a session built over
    // them subscribes again and is quoted again. Losing and rebuilding the
    // connections is phase 105's, and the farm reconnect suite's.
    phase!("--- Phase 109: Fallback Resubscribe Under A New Session ({} — session-independent) ---", ALWAYS_QUOTING.0);

    // As the venue named it, in the phase before this one. Written in here, a
    // contract id that had changed or that this account sees differently read
    // as a subscription that produced nothing.
    let Some(&con_id) = FALLBACK_CON_ID.get() else {
        skipped!("  SKIP: the phase that asks the venue for {} did not name it\n", ALWAYS_QUOTING.0);
        return conns;
    };

    // Step 1: subscribe and read a quote
    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );

    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id, symbol: ALWAYS_QUOTING.0.into(),
                // Named, not left to the default. A subscription with no
                // exchange is asked for on BEST, and the venue refuses a
                // crypto there: "BEST/CRYPTO/Top". The ticks still arrive and
                // carry no prices, so the phase saw a stream and no quote.
                exchange: ALWAYS_QUOTING.2.into(), sec_type: ALWAYS_QUOTING.1.into(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    let join = run_hot_loop(hot_loop);

    let before = quoted_tick(&event_rx, &shared, Duration::from_secs(15));

    let conns1 = shutdown_and_reclaim(&control_tx, join, account_id.clone());

    let (bid, ask) = before.unwrap_or_else(|| {
        panic!(
            "no quote for {} before the disconnect, so whatever resumes after it \
             is not this subscription coming back",
            ALWAYS_QUOTING.0
        )
    });
    println!("  Step 1: quote before disconnect bid={bid:.5} ask={ask:.5}");

    // Step 2: Reconnect and verify ticks resume
    let shared2 = Arc::new(SharedState::new());
    let (event_tx2, event_rx2) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop2, control_tx2) = HotLoop::with_connections(
        shared2.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx2, Default::default())),
        conns1.account_id.clone(),
        conns1.farm,
        conns1.ccp,
        conns1.hmds,
        None,
    );

    control_tx2
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id, symbol: ALWAYS_QUOTING.0.into(),
                // Named, not left to the default. A subscription with no
                // exchange is asked for on BEST, and the venue refuses a
                // crypto there: "BEST/CRYPTO/Top". The ticks still arrive and
                // carry no prices, so the phase saw a stream and no quote.
                exchange: ALWAYS_QUOTING.2.into(), sec_type: ALWAYS_QUOTING.1.into(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    let join2 = run_hot_loop(hot_loop2);

    let after = quoted_tick(&event_rx2, &shared2, Duration::from_secs(15));

    let conns2 = shutdown_and_reclaim(&control_tx2, join2, conns1.account_id);

    let (bid, ask) = after.unwrap_or_else(|| {
        panic!("no quote for {} after reconnecting", ALWAYS_QUOTING.0)
    });
    println!("  Step 2: quote after reconnect bid={bid:.5} ask={ask:.5}");
    println!("  PASS\n");
    conns2
}

// ─── Phase 110: High-frequency tick stress test ───

pub(super) fn phase_tick_stress_test(conns: Conns) -> Conns {
    phase!("--- Phase 110: High-Frequency Tick Stress Test (SPY+AAPL+MSFT, 30s) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );

    // Subscribe to 3 high-volume instruments
    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id: 756733, symbol: "SPY".into(), exchange: String::new(), sec_type: "STK".into(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id: 265598, symbol: "AAPL".into(), exchange: String::new(), sec_type: String::new(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id: 272093, symbol: "MSFT".into(), exchange: String::new(), sec_type: String::new(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    let join = run_hot_loop(hot_loop);

    let run_duration = Duration::from_secs(30);
    let deadline = Instant::now() + run_duration;
    let mut total_ticks = 0u64;
    let mut per_instrument = [0u64; 3]; // SPY, AAPL, MSFT
    let mut last_timestamp = [0u64; 3];
    let mut monotonic_violations = 0u32;
    let mut first_tick = false;

    while Instant::now() < deadline {
        if let Ok(Event::Tick(instrument)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            total_ticks += 1;
            let idx = instrument as usize;
            if idx < 3 {
                per_instrument[idx] += 1;
                let q = shared.market.quote(instrument);
                if q.timestamp_ns > 0 && q.timestamp_ns < last_timestamp[idx] {
                    monotonic_violations += 1;
                }
                if q.timestamp_ns > 0 {
                    last_timestamp[idx] = q.timestamp_ns;
                }
            }
            if !first_tick {
                first_tick = true;
                println!(
                    "  First tick at +{:.1}s",
                    run_duration.as_secs_f64() - (deadline - Instant::now()).as_secs_f64()
                );
            }
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if !first_tick {
        no_market(&shared, "no ticks in 30s");
        return conns;
    }

    let elapsed = run_duration.as_secs_f64();
    let rate = total_ticks as f64 / elapsed;
    println!("  Total ticks: {total_ticks} ({rate:.1}/sec)");
    println!(
        "  SPY={} AAPL={} MSFT={}",
        per_instrument[0], per_instrument[1], per_instrument[2]
    );
    println!("  Monotonic violations: {monotonic_violations}");

    // Whether one session carries several streams at once. These three are US
    // stocks, so outside the regular session a quiet one is the hour and not
    // the client: measured after hours, the liquid one drew thirty-two ticks
    // in thirty seconds and the other two drew none. Asked of three instruments
    // that quote around the clock — three currency pairs on the same session —
    // all three answered, at over a hundred and fifty ticks each. So the
    // question this asks is only answerable while these three trade.
    let instruments_with_ticks = per_instrument.iter().filter(|&&c| c > 0).count();
    let (session, _) = market_session();
    if session == MarketSession::Regular {
        assert!(
            instruments_with_ticks >= 2,
            "one session must carry more than one stream, got {instruments_with_ticks}"
        );
    } else if instruments_with_ticks < 2 {
        skipped!(
            "  SKIP: {session:?} — {instruments_with_ticks} of three quoted; \
             these three are US stocks\n",
        );
    }
    assert_eq!(
        monotonic_violations, 0,
        "Timestamps should be monotonically increasing"
    );

    println!("  PASS\n");
    conns
}

// ─── Phase 126: TBT Subscribe + Unsubscribe lifecycle ───

pub(super) fn phase_tbt_unsubscribe(conns: Conns) -> Conns {
    phase!("--- Phase 126: TBT Subscribe + Unsubscribe (SPY via HMDS) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );

    control_tx
        .send(ControlCommand::SubscribeTbt {
            contract: ContractRef { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".to_string(), exchange: "SMART".to_string(), ..Default::default() },
            req_id: 1,
            number_of_ticks: 0,
            ignore_size: false,
            tbt_type: TbtType::Last,
            reply_tx: None,
        })
        .unwrap();
    let join = run_hot_loop(hot_loop);

    // Step 1: Wait for at least one TBT event
    let mut tbt_before = 0u32;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::TbtTrade(_)) | Ok(Event::TbtQuote(_)) => {
                tbt_before += 1;
                if tbt_before >= 3 {
                    break;
                }
            }
            _ => {}
        }
    }

    if tbt_before == 0 {
        let conns = shutdown_and_reclaim(&control_tx, join, account_id);
        no_market(&shared, "no tick-by-tick data");
        return conns;
    }
    println!(
        "  Step 1: {tbt_before} TBT events received before unsubscribe"
    );

    // Step 2: Unsubscribe — instrument 0 is the first registered (SPY)
    control_tx
        .send(ControlCommand::UnsubscribeTbt { req_id: 1, instrument: 0 })
        .unwrap();
    // What matters is that the stream stops, not how much of it was already on
    // its way. A payload in flight when the withdrawal goes out still arrives,
    // and on a busy contract that is several ticks, not one — this counted them
    // against a fixed ceiling that had never been reached, because until the
    // payload was read at all the count was always zero.
    //
    // So: let what is in flight land, then require silence.
    std::thread::sleep(Duration::from_secs(3));
    let mut in_flight = 0u32;
    while let Ok(ev) = event_rx.try_recv() {
        match ev {
            Event::TbtTrade(_) | Event::TbtQuote(_) => in_flight += 1,
            _ => {}
        }
    }

    let settle = Instant::now() + Duration::from_secs(5);
    let mut tbt_after = 0u32;
    while Instant::now() < settle {
        if let Ok(Event::TbtTrade(_) | Event::TbtQuote(_)) =
            event_rx.recv_timeout(Duration::from_millis(200))
        {
            tbt_after += 1;
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    println!(
        "  Step 2: {in_flight} in flight, then {tbt_after} in the five seconds after"
    );
    assert_eq!(
        tbt_after, 0,
        "the withdrawal did not stop the stream: {tbt_after} ticks arrived five seconds on",
    );
    println!("  PASS\n");
    conns
}

// ─── Phase 128: TBT + Regular Quotes Dual Stream ───

pub(super) fn phase_tbt_and_quotes_dual_stream(conns: Conns) -> Conns {
    phase!("--- Phase 128: TBT + Regular Quotes Dual Stream (SPY) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );

    // Subscribe to both regular market data and TBT simultaneously
    control_tx
        .send(ControlCommand::Subscribe {
            contract: ContractRef { con_id: 756733, symbol: "SPY".into(), exchange: String::new(), sec_type: "STK".into(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
            generic_ticks: Vec::new(), issued: 0,
        })
        .unwrap();
    control_tx
        .send(ControlCommand::SubscribeTbt {
            contract: ContractRef { con_id: 756733, symbol: "SPY".into(), sec_type: "STK".to_string(), exchange: "SMART".to_string(), ..Default::default() },
            req_id: 1,
            number_of_ticks: 0,
            ignore_size: false,
            tbt_type: TbtType::Last,
            reply_tx: None,
        })
        .unwrap();
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut tick_count = 0u32;
    let mut tbt_trade_count = 0u32;
    let mut tbt_quote_count = 0u32;
    let mut got_tick = false;
    let mut got_tbt = false;

    while Instant::now() < deadline {
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Tick(instrument)) => {
                tick_count += 1;
                if !got_tick {
                    let q = shared.market.quote(instrument);
                    println!(
                        "  First regular tick: bid={:.4} ask={:.4}",
                        q.bid as f64 / PRICE_SCALE as f64,
                        q.ask as f64 / PRICE_SCALE as f64
                    );
                    got_tick = true;
                }
            }
            Ok(Event::TbtTrade(trade)) => {
                tbt_trade_count += 1;
                if !got_tbt {
                    println!(
                        "  First TBT trade: price={:.4} size={}",
                        trade.price as f64 / PRICE_SCALE as f64,
                        trade.size
                    );
                    got_tbt = true;
                }
            }
            Ok(Event::TbtQuote(quote)) => {
                tbt_quote_count += 1;
                if !got_tbt {
                    println!(
                        "  First TBT quote: bid={:.4} ask={:.4}",
                        quote.bid as f64 / PRICE_SCALE as f64,
                        quote.ask as f64 / PRICE_SCALE as f64
                    );
                    got_tbt = true;
                }
            }
            _ => {}
        }
        // Wait for both streams to produce data
        if got_tick && got_tbt && tick_count >= 5 && (tbt_trade_count + tbt_quote_count) >= 3 {
            break;
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if !got_tick && !got_tbt {
        no_market(&shared, "no data on either stream");
        return conns;
    }

    println!(
        "  Regular ticks: {tick_count}  TBT trades: {tbt_trade_count}  TBT quotes: {tbt_quote_count}"
    );

    // This phase requires both streams at once; one stream working says nothing
    // about the two running together.
    if got_tick && got_tbt {
        println!("  PASS (both streams active simultaneously)\n");
    } else if got_tick {
        skipped!("  SKIP: regular ticks only — nothing came on tick-by-tick to run beside them\n");
    } else {
        skipped!("  SKIP: tick-by-tick only — no regular ticks to run beside it\n");
    }
    conns
}

// ─── Phase 129: Concurrent Subscribe Stress (10 instruments) ───

pub(super) fn phase_concurrent_subscribe_stress(conns: Conns) -> Conns {
    phase!("--- Phase 129: Concurrent Subscribe Stress (10 instruments, 20s) ---");

    let account_id = conns.account_id;
    let shared = Arc::new(SharedState::new());
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(4096);
    let (hot_loop, control_tx) = HotLoop::with_connections(
        shared.clone(),
        Some(ibx::engine::hot_loop::EventSink::new(event_tx, Default::default())),
        account_id.clone(),
        conns.farm,
        conns.ccp,
        conns.hmds,
        None,
    );

    // 10 well-known US stock con_ids (IB paper account should have market data for all)
    let instruments: &[(i64, &str)] = &[
        (756733, "SPY"),      // S&P 500 ETF
        (265598, "AAPL"),     // Apple
        (272093, "MSFT"),     // Microsoft
        (208813720, "GOOGL"), // Alphabet
        (15124833, "AMZN"),   // Amazon
        (107113386, "META"),  // Meta
        (76792991, "TSLA"),   // Tesla
        (4815747, "NVDA"),    // Nvidia
        (6459, "AMD"),        // AMD
        (267321477, "NFLX"),  // Netflix
    ];

    // Subscribe to all 10 simultaneously
    for &(con_id, symbol) in instruments {
        control_tx
            .send(ControlCommand::Subscribe {
                contract: ContractRef { con_id, symbol: symbol.into(), exchange: String::new(), sec_type: String::new(), currency: String::new(), last_trade_date: String::new(), strike: 0.0, right: String::new(), multiplier: String::new() }, filters: Default::default(), mode_9887: 0, regulatory_snapshot: false, reply_tx: None,
                generic_ticks: Vec::new(), issued: 0,
            })
            .unwrap();
    }
    let join = run_hot_loop(hot_loop);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut per_instrument = std::collections::HashMap::new();
    let mut total_ticks = 0u64;

    while Instant::now() < deadline {
        if let Ok(Event::Tick(instrument)) = event_rx.recv_timeout(Duration::from_millis(100)) {
            total_ticks += 1;
            *per_instrument.entry(instrument).or_insert(0u64) += 1;
        }
        // Stop early once ticks have arrived from at least 5 instruments
        if per_instrument.len() >= 5 && total_ticks >= 50 {
            break;
        }
    }

    let conns = shutdown_and_reclaim(&control_tx, join, account_id);

    if total_ticks == 0 {
        no_market(&shared, "no ticks");
        return conns;
    }

    println!(
        "  Total ticks: {} across {} instruments",
        total_ticks,
        per_instrument.len()
    );
    for (&inst, &count) in &per_instrument {
        println!("    instrument {inst} → {count} ticks");
    }

    // At least 3 instruments should receive ticks when 10 are subscribed
    assert!(
        per_instrument.len() >= 3,
        "Expected ticks from >=3 instruments, got {}",
        per_instrument.len()
    );
    println!("  PASS\n");
    conns
}
