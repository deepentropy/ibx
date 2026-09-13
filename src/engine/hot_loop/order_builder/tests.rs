//! The tests for this module.
//!
//! One file per module, as `api/client` already does it. Each block below
//! reaches the code it tests through `super::super`, which is the module this
//! file belongs to.


/// A stock is named by its symbol. Everything else names one contract by IB's
/// local symbol, which distinguishes a member of a family from the family.
#[test]
fn only_a_stock_leaves_the_contract_unnamed() {
    for (sec_type, key, wants_id) in [
        // expiry|strike|right|multiplier|tradingClass|localSymbol
        ("STK", "|0|||XCLASS|XLOCAL", false),
        ("IND", "|0|||XCLASS|XLOCAL", true),
        ("CFD", "|0|||XCLASS|XLOCAL", true),
        ("CRYPTO", "|0|||XCLASS|XLOCAL", true),
    ] {
        let mut context = Context::new();
        let instrument = context
            .market
            .try_register_contract(1, "X", sec_type, "SMART", key)
            .expect("register a contract");
        context.set_symbol(instrument, "X".to_string());
        let mut fields: Vec<(u32, String)> = Vec::new();
        push_contract_identity(&mut fields, &context, instrument);
        let named = fields.iter().any(|(t, _)| *t == 48);
        assert_eq!(named, wants_id, "{sec_type} names the contract: {fields:?}");
        assert!(
            !(wants_id && fields.iter().any(|(t, _)| *t == 6058)),
            "{sec_type} states no trading class: {fields:?}",
        );
    }
}

/// The two-part midpoint peg is a distinct order type, and a replace states
/// the type on its own message. The attributes settled it in place on a list
/// the replace does not send, so a replaced two-part peg went out as a bare
/// `P` with the instruction that names the peg dropped — and `P` alone is
/// three different orders depending on an ExecInst that was no longer there.
#[test]
fn a_replaced_two_part_peg_keeps_its_order_type() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());

    let attrs = crate::types::OrderAttrs {
        mid_offset_at_whole: 0.01,
        mid_offset_at_half: 0.005,
        ..Default::default()
    };
    context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
        order_id: 42,
        instrument,
        side: Side::Buy,
        qty: 100 * crate::types::QTY_SCALE,
        kind: crate::types::OrderKind::PegMid { offset: 0, price_cap: 0 },
        tif: b'0',
        attrs,
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let mut buf = [0u8; 8192];
    let n = peer.read(&mut buf).unwrap();
    let placed = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(placed.contains("\u{1}40=PMID2\u{1}"), "placed as the two-part form: {placed}");

    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 42,
        price: 0,
        qty: 100 * crate::types::QTY_SCALE,
        outside_rth: false,
        ord_type: 0,
        tif: 0,
        stop_price: 0,
        spec: None,
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("35=").as_deref(), Some("G"), "a replace was sent: {msg}");
    assert_eq!(tag("40=").as_deref(), Some("PMID2"), "it is still the two-part peg: {msg}");
}

/// A replace names the contract by its local symbol, and an option's local
/// symbol is not its underlying's. With no local symbol carried on the
/// instrument the tag is left off rather than filled with the underlying —
/// the venue tolerates an omission and acts on a name, and the name of the
/// family is the wrong contract to act on.
#[test]
fn a_replace_does_not_name_an_option_by_its_underlying() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context
        .market
        .try_register_contract(756733, "SPY", "OPT", "SMART", "20270917|500|C|100|||USD")
        .expect("register an option");
    context.set_symbol(instrument, "SPY".to_string());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());

    context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
        order_id: 42,
        instrument,
        side: Side::Buy,
        qty: crate::types::QTY_SCALE,
        kind: crate::types::OrderKind::Limit { price: 5 * crate::types::PRICE_SCALE },
        tif: b'0',
        attrs: Default::default(),
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let mut buf = [0u8; 8192];
    let _ = peer.read(&mut buf).unwrap();

    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 42,
        price: 6 * crate::types::PRICE_SCALE,
        qty: crate::types::QTY_SCALE,
        outside_rth: false,
        ord_type: 0,
        tif: 0,
        stop_price: 0,
        spec: None,
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("35=").as_deref(), Some("G"), "a replace was sent: {msg}");
    assert_eq!(tag("6035=").as_deref(), None, "it does not name the underlying: {msg}");
}

/// A replace is a full statement of the order, so an attribute the submit
/// made survives it. This one came back without its all-or-none instruction
/// and was a different order to the one the caller had placed.
#[test]
fn a_replace_restates_the_attributes_the_order_was_placed_with() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());

    let attrs = crate::types::OrderAttrs { all_or_none: true, ..Default::default() };
    context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
        order_id: 42,
        instrument,
        side: Side::Buy,
        qty: 100 * crate::types::QTY_SCALE,
        kind: crate::types::OrderKind::Limit { price: 150 * crate::types::PRICE_SCALE },
        tif: b'0',
        attrs,
    });
    drain_and_send_orders(
        &mut conn,
        &mut context,
        "DU1",
        &mut hb,
        false,
        &shared,
        false,
        &None,
    );
    let mut buf = [0u8; 8192];
    let n = peer.read(&mut buf).unwrap();
    let placed = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(placed.contains("\u{1}18=G\u{1}"), "the order was placed all-or-none: {placed}");

    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 42,
        price: 151 * crate::types::PRICE_SCALE,
        qty: 100 * crate::types::QTY_SCALE,
        outside_rth: false,
        ord_type: 0,
        tif: 0,
        stop_price: 0,
        spec: None,
    });
    drain_and_send_orders(
        &mut conn,
        &mut context,
        "DU1",
        &mut hb,
        false,
        &shared,
        false,
        &None,
    );
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("35=").as_deref(), Some("G"), "a replace was sent: {msg}");
    assert_eq!(tag("6035=").as_deref(), Some("SPY"), "it names the contract: {msg}");
    assert_eq!(tag("18=").as_deref(), Some("G"), "it is still all-or-none: {msg}");
    assert_eq!(msg.matches("\u{1}38=").count(), 1, "the quantity is stated once: {msg}");
}

/// The five fields a cancel always names, and the one it never does. Two
/// cancels of the same order must also name themselves differently, or the
/// retry is a duplicate the server is free to drop.
#[test]
fn a_cancel_names_the_side_account_and_originator_but_no_transact_time() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.insert_order(crate::types::Order::new(
        42,
        instrument,
        Side::Sell,
        100 * crate::types::QTY_SCALE,
        150 * crate::types::PRICE_SCALE,
        b'2',
        b'0',
        0,
    ));
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());

    let mut names = Vec::new();
    for _ in 0..2 {
        context.pending_orders.push(crate::types::OrderRequest::Cancel { order_id: 42 });
        drain_and_send_orders(
            &mut conn,
            &mut context,
            "DU1",
            &mut hb,
            false,
            &shared,
            false,
            &None,
        );
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]).to_string();
        let tag =
            |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

        assert_eq!(tag("35=").as_deref(), Some("F"), "a cancel was sent: {msg}");
        assert_eq!(tag("41=").as_deref(), Some("42.0"), "the order it cancels: {msg}");
        assert_eq!(tag("54=").as_deref(), Some("2"), "the side it carries: {msg}");
        assert_eq!(tag("1=").as_deref(), Some("DU1"), "the account: {msg}");
        assert_eq!(tag("6088=").as_deref(), Some("Socket"), "the originator: {msg}");
        assert_eq!(tag("38=").as_deref(), Some("100"), "what it cancels: {msg}");
        assert_eq!(tag("6008=").as_deref(), Some("756733"), "the contract it cancels: {msg}");
        assert_eq!(tag("60="), None, "no transact time is written in the order path: {msg}");
        names.push(tag("11=").expect("a cancel names itself"));
    }
    assert_ne!(names[0], names[1], "a retried cancel needs its own name: {names:?}");
}

 ///,: the replace restated the tracked order's type,
/// time-in-force and trigger, so a caller changing any of them had the
/// change accepted, acknowledged and dropped. Asserted on the bytes,
/// because the request-level tests passed throughout.
#[test]
fn a_modify_states_the_type_tif_and_trigger_it_carries() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    // Resting: a DAY limit with no trigger.
    context.insert_order(crate::types::Order::new(
        42,
        instrument,
        Side::Buy,
        100 * crate::types::QTY_SCALE,
        150 * crate::types::PRICE_SCALE,
        b'2',
        b'0',
        0,
    ));

    // Modified to a GTC stop at 149.
    context.modify_ex(
        42,
        150 * crate::types::PRICE_SCALE,
        100,
        false,
        b'3',
        b'1',
        149 * crate::types::PRICE_SCALE,
    );
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(
        &mut conn,
        &mut context,
        "DU1",
        &mut hb,
        false,
        &shared,
        false,
        &None,
    );

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]);
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

    assert_eq!(tag("35=").as_deref(), Some("G"), "a replace was sent: {msg}");
    assert_eq!(tag("40=").as_deref(), Some("3"), "the type the caller stated: {msg}");
    assert_eq!(tag("59=").as_deref(), Some("1"), "the tif the caller stated: {msg}");
    assert_eq!(
        tag("99="),
        Some(format_price(149 * crate::types::PRICE_SCALE).to_string()),
        "the trigger the caller stated: {msg}"
    );
    // Where the resting order is working. Left off, the venue compares the
    // replace against the order it holds and refuses it as a mismatch on
    // this field, naming a tag number the caller has never heard of; the
    // order then sits inactive and the caller's own cancel finds nothing.
    assert_eq!(tag("100=").as_deref(), Some("BEST"), "the destination: {msg}");
    assert_eq!(tag("6210=").as_deref(), Some("BEST"), "and its second statement: {msg}");
}

/// A moved trigger is sent as stated, on the contract's tick grid or not.
/// The venue rejects an off-grid price rather than adjusting it.
#[test]
fn a_moved_trigger_is_sent_at_the_price_the_caller_stated() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.market.set_min_tick(instrument, 0.05);
    context.insert_order(crate::types::Order::new(
        42,
        instrument,
        Side::Sell,
        100 * crate::types::QTY_SCALE,
        150 * crate::types::PRICE_SCALE,
        b'3',
        b'0',
        149 * crate::types::PRICE_SCALE,
    ));

    // 149.03 is off a five-cent grid.
    let off_grid = 149 * crate::types::PRICE_SCALE + 3 * crate::types::PRICE_SCALE / 100;
    context.modify_ex(42, 150 * crate::types::PRICE_SCALE, 100, false, b'3', b'0', off_grid);
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(
        &mut conn,
        &mut context,
        "DU1",
        &mut hb,
        false,
        &shared,
        false,
        &None,
    );

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]);
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(
        tag("99="),
        Some(format_price(off_grid).to_string()),
        "the trigger the caller stated, unmoved: {msg}",
    );
}

/// A cancel names a version of an order the recovery may be about to
/// correct. Sent against a reconnect that has not finished accounting for
/// what the broker holds, it states a version that may not exist there and
/// is refused, leaving the order live.
#[test]
fn a_cancel_waits_for_the_recovery_to_say_what_the_broker_holds() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.insert_order(crate::types::Order::new(
        42,
        instrument,
        Side::Buy,
        100 * crate::types::QTY_SCALE,
        150 * crate::types::PRICE_SCALE,
        b'2',
        b'1',
        0,
    ));
    // This is the order in doubt: a write for it failed, so what the broker
    // holds for it is exactly what the recovery is about to say.
    context.set_order_status_forced(42, crate::types::OrderStatus::Uncertain);
    context.pending_orders.push(crate::types::OrderRequest::Cancel { order_id: 42 });
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());

    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, true, &None);
    peer.set_read_timeout(Some(std::time::Duration::from_millis(50))).unwrap();
    let mut buf = [0u8; 512];
    assert!(
        std::io::Read::read(&mut peer, &mut buf).unwrap_or(0) == 0,
        "nothing goes out while the recovery is still settling",
    );

    // An order placed since the reconnect is in no doubt, so its own cancel
    // is not made to wait on a recovery that has nothing to do with it.
    context.insert_order(crate::types::Order::new(
        43,
        instrument,
        Side::Buy,
        crate::types::QTY_SCALE,
        150 * crate::types::PRICE_SCALE,
        b'2',
        b'1',
        0,
    ));
    context.pending_orders.push(crate::types::OrderRequest::Cancel { order_id: 43 });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, true, &None);
    let n = std::io::Read::read(&mut peer, &mut buf).unwrap_or(0);
    assert!(
        String::from_utf8_lossy(&buf[..n]).contains("35=F"),
        "the cancel for the order that is not in doubt goes now",
    );

    // Once it has settled, the held one goes too.
    drain_and_send_orders(
        &mut conn,
        &mut context,
        "DU1",
        &mut hb,
        false,
        &shared,
        false,
        &None,
    );
    let n = std::io::Read::read(&mut peer, &mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).contains("35=F"), "and then it is sent",);
}

/// Every leg a bracket writes is recorded as this client's own.
///
/// Recorded under the id the request is addressed by alone, the two children
/// read afterwards as orders nobody here placed: a bust or a correction for one
/// of them, arriving while a caller was asking what the venue has finished, was
/// filed as history — it took nothing back, and the position it was undoing
/// stayed where it was.
#[test]
fn every_leg_a_bracket_writes_is_recorded_as_this_client_s_own() {
    let (client, peer) = crate::protocol::connection::Connection::for_test();
    let mut conn = Some(client);
    let _peer = peer;
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.pending_orders.push(crate::types::OrderRequest::SubmitBracket { con_id: 0,
        parent_id: 10,
        tp_id: 11,
        sl_id: 12,
        instrument,
        side: Side::Buy,
        qty: 100 * crate::types::QTY_SCALE,
        entry_price: 150 * crate::types::PRICE_SCALE,
        take_profit: 155 * crate::types::PRICE_SCALE,
        stop_loss: 145 * crate::types::PRICE_SCALE,
    });

    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    let (tx, rx) = std::sync::mpsc::sync_channel(4096);
    let _rx = rx;
    drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false,
        &Some(crate::engine::hot_loop::EventSink::new(tx, Default::default())),
    );

    for id in [10u64, 11, 12] {
        assert!(
            shared.orders.the_order_went_out(id),
            "leg {id} went out from here, so a report about it is not history",
        );
    }
}

/// A bracket is three messages and one outcome. All three are written
/// whatever any one of them returns, so a failure leaves every leg in a
/// state the wire never confirmed — and a child still reported as working
/// is an entry whose exits may not exist.
#[test]
fn a_bracket_whose_write_failed_leaves_no_leg_reported_as_working() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.pending_orders.push(crate::types::OrderRequest::SubmitBracket { con_id: 0,
        parent_id: 10,
        tp_id: 11,
        sl_id: 12,
        instrument,
        side: Side::Buy,
        qty: 100 * crate::types::QTY_SCALE,
        entry_price: 150 * crate::types::PRICE_SCALE,
        take_profit: 155 * crate::types::PRICE_SCALE,
        stop_loss: 145 * crate::types::PRICE_SCALE,
    });

    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    let (tx, rx) = std::sync::mpsc::sync_channel(4096);
    drain_and_send_orders(
        &mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &Some(crate::engine::hot_loop::EventSink::new(tx, Default::default())),
    );

    for id in [10u64, 11, 12] {
        assert_eq!(
            context.order(id).map(|o| o.status),
            Some(OrderStatus::Uncertain),
            "leg {id} was written and its outcome is not known",
        );
    }

    let announced: Vec<u64> = rx
        .try_iter()
        .filter_map(|e| match e {
            crate::bridge::Event::OrderUpdate(u)
                if u.status == OrderStatus::Uncertain => Some(u.order_id),
            _ => None,
        })
        .collect();
    for id in [10u64, 11, 12] {
        assert!(announced.contains(&id), "leg {id} was not announced");
    }
}

/// A write that fails has not established that the broker has nothing —
/// the transport says as much of TLS. Calling it a rejection invited a
/// resubmission of an order that may be working.
#[test]
fn an_order_whose_write_failed_is_unknown_rather_than_rejected() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    // Closing this side's write half makes the send fail on the call
    // rather than after a buffer fills.
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.insert_order(crate::types::Order::new(
        42,
        instrument,
        Side::Buy,
        100 * crate::types::QTY_SCALE,
        150 * crate::types::PRICE_SCALE,
        b'2',
        b'1',
        0,
    ));
    context.last_clord.insert(42, "42.7".to_string());
    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 42,
        price: 151 * crate::types::PRICE_SCALE,
        qty: 100 * crate::types::QTY_SCALE,
        outside_rth: false,
        ord_type: 0,
        tif: 0,
        stop_price: 0,
        spec: None,
    });

    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    let (tx, rx) = std::sync::mpsc::sync_channel(4096);
    drain_and_send_orders(
        &mut conn,
        &mut context,
        "DU1",
        &mut hb,
        false,
        &shared,
        false,
        &Some(crate::engine::hot_loop::EventSink::new(tx, Default::default())),
    );

    // Both deliveries, because the event channel is documented as a second
    // delivery of everything rather than a lesser one — and an order whose
    // state is no longer known is the last thing to deliver only once.
    let events: Vec<_> = rx.try_iter().collect();
    assert!(
        events.iter().any(|e| matches!(e, crate::bridge::Event::OrderUpdate(u)
            if u.order_id == 42 && u.status == crate::types::OrderStatus::Uncertain)),
        "a caller reading events is told too: {events:?}",
    );

    let updates = shared.orders.drain_order_updates();
    assert!(
        updates
            .iter()
            .any(|u| u.order_id == 42 && u.status == crate::types::OrderStatus::Uncertain),
        "the caller is told, and told it is unknown: {updates:?}",
    );
    assert!(
        !updates.iter().any(|u| u.status == crate::types::OrderStatus::Rejected),
        "not that the broker refused it: {updates:?}",
    );
    assert!(
        context.order(42).is_some(),
        "and it stays tracked, for the recovery to account for",
        );
    let kept = context.order(42).unwrap();
    assert_eq!(
        kept.tif, b'1',
        "holding what the broker was last known to hold, not what the \
         replace tried to make it: the attempt was never accepted",
    );
    assert_eq!(kept.price, 150 * crate::types::PRICE_SCALE, "nor its price");
    assert!(context.order(43).is_none(), "and the attempt itself is not tracked");
}

/// A replace names the order the broker knows, and a second replace names
/// what the first one left it under. Stating anything else is an
/// OrigClOrdID the broker has never seen, which it refuses.
#[test]
fn a_replacement_can_itself_be_replaced() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.insert_order(crate::types::Order::new(
        7,
        instrument,
        Side::Buy,
        crate::types::QTY_SCALE,
        crate::types::PRICE_SCALE,
        b'2',
        b'0',
        0,
    ));
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    let mut buf = [0u8; 4096];

    // The same order stepped up twice, as an ibapi caller does it: one id
    // throughout, and the version is what advances.
    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 7,
        price: 2 * crate::types::PRICE_SCALE,
        qty: crate::types::QTY_SCALE,
        outside_rth: false,
        ord_type: 0,
        tif: 0,
        stop_price: 0,
        spec: None,
    });
    drain_and_send_orders(
        &mut conn,
        &mut context,
        "DU1",
        &mut hb,
        false,
        &shared,
        false,
        &None,
    );
    let n = peer.read(&mut buf).unwrap();
    let first = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
    assert!(first.contains("|41=7.0|"), "the first replace names the original: {first}");

    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 7,
        price: 3 * crate::types::PRICE_SCALE,
        qty: crate::types::QTY_SCALE,
        outside_rth: false,
        ord_type: 0,
        tif: 0,
        stop_price: 0,
        spec: None,
    });
    drain_and_send_orders(
        &mut conn,
        &mut context,
        "DU1",
        &mut hb,
        false,
        &shared,
        false,
        &None,
    );
    let n = peer.read(&mut buf).unwrap();
    let second = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
    assert!(
        second.contains("|41=7.1|"),
        "the second names what the broker last acknowledged, not an id it never saw: {second}",
    );
}

/// What a pegged order actually puts on the wire.
#[test]
fn a_pegged_order_states_its_offset_and_no_limit_price() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
        order_id: 1,
        instrument,
        side: Side::Buy,
        qty: crate::types::QTY_SCALE,
        kind: crate::types::OrderKind::PegMkt { offset: 0, price_cap: 0 },
        tif: b'0',
        attrs: Default::default(),
    });
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(
        &mut conn,
        &mut context,
        "DU1",
        &mut hb,
        false,
        &shared,
        false,
        &None,
    );

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
    println!("PEGMKT WIRE: {msg}");
    assert!(msg.contains("|211=0|"), "the offset is stated: {msg}");
}

/// A replace may now change the order type, and a stop that becomes a
/// limit has no trigger to state. Carrying the resting one anyway put a
/// tag 99 on a limit order.
#[test]
fn a_replace_that_drops_the_trigger_does_not_carry_it() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    // A resting stop with a trigger at 149.
    context.insert_order(crate::types::Order::new(
        42,
        instrument,
        Side::Sell,
        100 * crate::types::QTY_SCALE,
        150 * crate::types::PRICE_SCALE,
        b'3',
        b'1',
        149 * crate::types::PRICE_SCALE,
    ));

    // Replaced as a plain limit at 151.
    context.modify_ex(42, 151 * crate::types::PRICE_SCALE, 100, false, b'2', 0, 0);
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(
        &mut conn,
        &mut context,
        "DU1",
        &mut hb,
        false,
        &shared,
        false,
        &None,
    );

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]);
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

    assert_eq!(tag("40=").as_deref(), Some("2"), "the stated type: {msg}");
    assert_eq!(
        tag("44=").as_deref(),
        Some(&*format_price(151 * crate::types::PRICE_SCALE)),
        "the limit price: {msg}"
    );
    assert_eq!(tag("99="), None, "and no trigger from the order it replaced: {msg}");
}

/// A modify that states none of them leaves what the resting order holds in
/// force, which is every caller that only moves a price or a quantity.
#[test]
fn a_modify_that_states_nothing_keeps_the_resting_values() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.insert_order(crate::types::Order::new(
        42,
        instrument,
        Side::Sell,
        100 * crate::types::QTY_SCALE,
        150 * crate::types::PRICE_SCALE,
        b'3',
        b'1',
        149 * crate::types::PRICE_SCALE,
    ));

    context.modify(42, 151 * crate::types::PRICE_SCALE, 100, false);
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(
        &mut conn,
        &mut context,
        "DU1",
        &mut hb,
        false,
        &shared,
        false,
        &None,
    );

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]);
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

    assert_eq!(tag("40=").as_deref(), Some("3"), "the resting type: {msg}");
    assert_eq!(tag("59=").as_deref(), Some("1"), "the resting tif: {msg}");
    // A stop has one price and it is the trigger, so the single price the
    // caller passed can only have meant that. Leaving 149 in place would
    // put 151 on no tag at all and move nothing.
    assert_eq!(
        tag("99="),
        Some(format_price(151 * crate::types::PRICE_SCALE).to_string()),
        "the moved trigger: {msg}"
    );
    assert!(!msg.contains("\u{1}44="), "a stop states no limit price: {msg}");
}
use super::*;
use crate::types::Order;

fn order(oid: u64, filled: crate::types::Qty, status: OrderStatus) -> Order {
    Order {
        order_id: oid,
        instrument: 0,
        side: Side::Buy,
        price: 100,
        qty: 10 * crate::types::QTY_SCALE,
        filled,
        status,
        ord_type: b'2',
        tif: b'0',
        stop_price: 0,
    }
}

// An outbound cancel synthesizes the PendingCancel phase the
// server never sends for a normal cancel.
#[test]
fn synthesize_pending_cancel_updates_and_notifies() {
    let mut context = Context::new();
    let shared = Arc::new(SharedState::new());
    context.insert_order(order(7, 3 * crate::types::QTY_SCALE, OrderStatus::PartiallyFilled));

    synthesize_pending_cancel(&mut context, &shared, 7, &None);

    assert_eq!(context.order(7).unwrap().status, OrderStatus::PendingCancel);
    let updates = shared.orders.drain_order_updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].status, OrderStatus::PendingCancel);
    assert_eq!(updates[0].filled_qty, 3.0);
    assert_eq!(updates[0].remaining_qty, 7.0);
}

#[test]
fn synthesize_pending_cancel_skips_terminal_and_unknown_orders() {
    let mut context = Context::new();
    let shared = Arc::new(SharedState::new());
    // Late cancel racing a fill: the order is done, no phase to report.
    context.insert_order(order(8, 10 * crate::types::QTY_SCALE, OrderStatus::Filled));

    synthesize_pending_cancel(&mut context, &shared, 8, &None);
    synthesize_pending_cancel(&mut context, &shared, 999, &None);

    assert_eq!(context.order(8).unwrap().status, OrderStatus::Filled);
    assert!(shared.orders.drain_order_updates().is_empty());
}

/// Adaptive, algo and what-if orders reach their own encoders and still
/// carry the attribute block: outside-RTH, the parent link, the OCA group
/// and the caller's tif go on the wire rather than being accepted by the
/// API and dropped. Asserted on the bytes, which is where an encoder that
/// omits the block differs from one that does not.
#[test]
fn adaptive_wire_carries_the_attributes_and_keeps_its_algo_tags() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::Adaptive {
            price: 100 * crate::types::PRICE_SCALE,
            priority: crate::types::AdaptivePriority::Urgent,
        },
        b'1',
        bracket_child_attrs(),
    );
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

    assert_eq!(tag("6433=").as_deref(), Some("1"), "outside RTH missing: {msg}");
    assert_eq!(tag("6107=").as_deref(), Some("42.0"), "parent link missing: {msg}");
    assert_eq!(tag("583=").as_deref(), Some("bracket_1"), "OCA group missing: {msg}");
    assert_eq!(tag("59=").as_deref(), Some("1"), "tif must be GTC, not DAY: {msg}");

    // And everything the standalone encoder emitted is unchanged.
    assert_eq!(tag("40=").as_deref(), Some("2"));
    assert_eq!(tag("18=").as_deref(), Some("e"), "adaptive wrapper missing: {msg}");
    assert_eq!(tag("847=").as_deref(), Some("Adaptive"));
    assert_eq!(tag("5957=").as_deref(), Some("1"));
    assert_eq!(tag("5958=").as_deref(), Some("adaptivePriority"));
    assert_eq!(tag("5960=").as_deref(), Some("Urgent"));
    assert!(
        msg.find("15=").unwrap() < msg.find("847=").unwrap(),
        "the strategy tags keep their position after the contract block: {msg}"
    );
}

#[test]
fn algo_wire_carries_the_attributes_and_keeps_its_algo_tags() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::Algo {
            price: 100 * crate::types::PRICE_SCALE,
            algo: AlgoParams::Vwap {
                max_pct_vol: Some("0.25".into()),
                no_take_liq: Some(true),
                allow_past_end_time: Some(false),
                start_time: Some(String::new()),
                end_time: Some(String::new()),
            },
        },
        b'1',
        bracket_child_attrs(),
    );
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

    assert_eq!(tag("6433=").as_deref(), Some("1"), "outside RTH missing: {msg}");
    assert_eq!(tag("6107=").as_deref(), Some("42.0"), "parent link missing: {msg}");
    assert_eq!(tag("583=").as_deref(), Some("bracket_1"), "OCA group missing: {msg}");
    assert_eq!(tag("59=").as_deref(), Some("1"), "tif must be GTC, not DAY: {msg}");

    assert_eq!(tag("847=").as_deref(), Some("Vwap"));
    assert_eq!(tag("849=").as_deref(), Some("0.25"), "maxPctVol missing: {msg}");
    assert_eq!(tag("5957=").as_deref(), Some("4"), "param count: {msg}");
    assert_eq!(tag("5958=").as_deref(), Some("noTakeLiq"));
    assert_eq!(tag("5960=").as_deref(), Some("1"));
}

/// A number the caller wrote reaches the venue in the caller's spelling.
///
/// An algorithm's parameters are text on the wire. The reference client
/// forwards each value as it was given, and an unmodelled strategy has always
/// gone through here the same way. A modelled one parsed the value into a
/// number and wrote the number back out, so `5.0` went as `5` and `1e-05` as
/// `0.00001`: two spellings of one order, depending on whether this client
/// happened to know the strategy. The parse is this client's own check; the
/// text is what goes.
#[test]
fn an_algo_number_reaches_the_wire_as_the_caller_wrote_it() {
    use crate::types::model::TagValue;
    let tv = |tag: &str, value: &str| TagValue { tag: tag.into(), value: value.into() };
    let wire = |strategy: &str, params: &[TagValue]| {
        let algo = crate::client_core::parse_algo_params(strategy, params).unwrap();
        send_kind_for_test(
            crate::types::OrderKind::Algo { price: 100 * crate::types::PRICE_SCALE, algo },
            b'1',
            crate::types::OrderAttrs::default(),
        )
    };
    let pair = |key: &str, value: &str| format!("5958={key}\u{1}5960={value}\u{1}");

    let vwap = wire("Vwap", &[tv("maxPctVol", "5.0")]);
    let tag_849 = vwap.split('\u{1}').find_map(|f| f.strip_prefix("849="));
    assert_eq!(tag_849, Some("5.0"), "{vwap}");

    let pct = wire("PctVol", &[tv("pctVol", "1e-05")]);
    assert!(pct.contains(&pair("pctVol", "1e-05")), "{pct}");

    let ice = wire("DarkIce", &[tv("displaySize", "007")]);
    assert!(ice.contains(&pair("displaySize", "007")), "{ice}");

    // The unmodelled path, which this now agrees with.
    let named = wire("Other", &[tv("maxPctVol", "5.0")]);
    assert!(named.contains(&pair("maxPctVol", "5.0")), "{named}");
}

/// A number the caller did not state is not sent.
///
/// A VWAP placed without `maxPctVol` went out with `849=0`, and a PctVol
/// without `pctVol` with `pctVol=0`: a share of the volume the caller never
/// named, on a field that says how much of it the order may take. What the
/// venue makes of `0` there is not known here, and neither is its own
/// default; the reference client sends only what the caller listed, and so
/// does this now.
#[test]
fn an_algo_number_the_caller_did_not_state_is_not_sent() {
    use crate::types::model::TagValue;
    let tv = |tag: &str, value: &str| TagValue { tag: tag.into(), value: value.into() };
    let wire = |strategy: &str, params: &[TagValue]| {
        let algo = crate::client_core::parse_algo_params(strategy, params).unwrap();
        send_kind_for_test(
            crate::types::OrderKind::Algo { price: 100 * crate::types::PRICE_SCALE, algo },
            b'1',
            crate::types::OrderAttrs::default(),
        )
    };
    let tag = |msg: &str, t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

    let vwap = wire("Vwap", &[]);
    assert_eq!(tag(&vwap, "849="), None, "{vwap}");
    assert_eq!(tag(&vwap, "847=").as_deref(), Some("Vwap"), "the order still names its strategy: {vwap}");

    let pct = wire("PctVol", &[tv("noTakeLiq", "0"), tv("startTime", "20260101-09:30:00"), tv("endTime", "20260101-16:00:00")]);
    assert!(!pct.contains("5958=pctVol\u{1}"), "{pct}");
    assert_eq!(tag(&pct, "5957=").as_deref(), Some("3"), "the count is of what is sent: {pct}");
}

/// A flag, a time or a risk aversion the caller did not state is not sent.
///
/// A Twap placed with no parameters went out with `allowPastEndTime=0`,
/// `startTime=` and `endTime=`, and an Arrival Price with no riskAversion
/// with `Neutral`: claims the caller never made, on the same footing as the
/// `maxPctVol=0` that stopped being sent. A stated flag still goes as `1`/`0`.
/// The pair count is asserted as well as the absence: a count that disagrees
/// with the pairs is the kind of thing the venue refuses naming nothing.
#[test]
fn an_algo_flag_time_or_risk_aversion_the_caller_did_not_state_is_not_sent() {
    use crate::types::model::TagValue;
    let tv = |tag: &str, value: &str| TagValue { tag: tag.into(), value: value.into() };
    let wire = |strategy: &str, params: &[TagValue]| {
        let algo = crate::client_core::parse_algo_params(strategy, params).unwrap();
        send_kind_for_test(
            crate::types::OrderKind::Algo { price: 100 * crate::types::PRICE_SCALE, algo },
            b'1',
            crate::types::OrderAttrs::default(),
        )
    };
    let tag = |msg: &str, t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    let pair = |key: &str, value: &str| format!("5958={key}\u{1}5960={value}\u{1}");

    let twap = wire("Twap", &[]);
    assert_eq!(tag(&twap, "5957=").as_deref(), Some("0"), "{twap}");
    assert!(!twap.contains("5958="), "{twap}");

    let twap = wire("Twap", &[tv("allowPastEndTime", "true")]);
    assert_eq!(tag(&twap, "5957=").as_deref(), Some("1"), "{twap}");
    assert!(twap.contains(&pair("allowPastEndTime", "1")), "a stated flag still goes as 1/0: {twap}");
    assert!(!twap.contains("5958=startTime"), "{twap}");

    let arrival = wire("ArrivalPx", &[tv("forceCompletion", "0")]);
    assert_eq!(tag(&arrival, "5957=").as_deref(), Some("1"), "{arrival}");
    assert!(arrival.contains(&pair("forceCompletion", "0")), "{arrival}");
    assert!(!arrival.contains("5958=riskAversion"), "{arrival}");
    assert_eq!(tag(&arrival, "849="), None, "{arrival}");
}

/// Conditions are joined the way the caller joined them.
///
/// Each condition states how it joins the next: `a` for AND, `o` for OR. The
/// last joins nothing and states `n` whatever it holds, which is how the
/// counterpart writes it. Every condition but the last used to go as `a`, so
/// an order the caller joined with OR reached the venue joined with AND: not
/// refused, a different order.
#[test]
fn conditions_are_joined_the_way_the_caller_joined_them() {
    use crate::types::{OrderAttrs, OrderCondition, OrderKind};
    let price = |is_conjunction_connection: bool| OrderCondition::Price {
        con_id: 756733,
        exchange: "SMART".into(),
        price: 100 * crate::types::PRICE_SCALE,
        is_more: true,
        trigger_method: 0,
        is_conjunction_connection,
    };
    let joins = |conditions: Vec<OrderCondition>| -> Vec<String> {
        let msg = send_kind_for_test(
            OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
            b'1',
            OrderAttrs { conditions, ..Default::default() },
        );
        msg.split('\u{1}').filter_map(|f| f.strip_prefix("6137=").map(str::to_string)).collect()
    };
    assert_eq!(joins(vec![price(false), price(true)]), ["o", "n"], "OR, then the terminator");
    assert_eq!(joins(vec![price(true), price(false), price(true)]), ["a", "o", "n"]);
    assert_eq!(joins(vec![price(false)]), ["n"], "one condition joins nothing");
}

#[test]
fn what_if_wire_carries_the_attributes_and_keeps_its_preview_flag() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
        b'1',
        crate::types::OrderAttrs { what_if: true, ..bracket_child_attrs() },
    );
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

    assert_eq!(tag("6433=").as_deref(), Some("1"), "outside RTH missing: {msg}");
    assert_eq!(tag("6107=").as_deref(), Some("42.0"), "parent link missing: {msg}");
    assert_eq!(tag("59=").as_deref(), Some("1"), "tif must be GTC, not DAY: {msg}");
    assert_eq!(tag("6091=").as_deref(), Some("1"), "what-if flag missing: {msg}");
    assert!(
        msg.find("15=").unwrap() < msg.find("6091=").unwrap(),
        "the preview flag keeps its position after the contract block: {msg}"
    );
    assert_eq!(tag("40=").as_deref(), Some("2"), "a limit preview: {msg}");
}

/// A caller written against the reference client names the hedging
/// contract on the contract and again on the order. Sent twice, the
/// gateway reads the second as a correction of the first.
#[test]
fn the_hedging_contract_is_named_once() {
    let attrs = crate::types::OrderAttrs {
        delta_neutral_contract: Some(Box::new(crate::types::DeltaNeutralContractSpec {
            con_id: 265598,
            delta: 0.5,
            price: 100.0,
        })),
        delta_neutral: Some(Box::new(crate::types::DeltaNeutralAttrs {
            order_type: "MKT".into(),
            aux_price: 0,
            con_id: 265598,
        })),
        ..crate::types::OrderAttrs::default()
    };
    let msg = send_kind_for_test(
        crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
        b'1',
        attrs,
    );
    let stated = msg.split('\u{1}').filter(|f| f.starts_with("6150=")).count();
    assert_eq!(stated, 1, "the hedging contract is named once: {msg}");
    assert!(msg.contains("6150=265598"), "{msg}");
}

#[test]
fn a_market_preview_states_market_and_no_price() {
    // Previewing every order as a limit is refused outright by a security
    // that only trades at market.
    let msg = send_kind_for_test(
        crate::types::OrderKind::Market,
        b'1',
        crate::types::OrderAttrs { what_if: true, ..bracket_child_attrs() },
    );
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("40=").as_deref(), Some("1"), "a market preview: {msg}");
    assert_eq!(tag("44=").as_deref(), None, "a market order states no price: {msg}");
    assert_eq!(tag("6091=").as_deref(), Some("1"), "still a preview: {msg}");
}

fn bracket_child_attrs() -> crate::types::OrderAttrs {
    crate::types::OrderAttrs {
        parent_id: 42,
        oca_group_str: "bracket_1".to_string(),
        oca_type: 1,
        outside_rth: true,
        ..Default::default()
    }
}

/// The shared state an encoder reads a contract's own currency out of.
fn shared_for_test() -> std::sync::Arc<SharedState> {
    std::sync::Arc::new(SharedState::new())
}

/// Encode one kind and return the frame as text.
fn send_kind_for_test(
    kind: crate::types::OrderKind,
    tif: u8,
    attrs: crate::types::OrderAttrs,
) -> String {
    use std::io::Read;
    let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut context = Context::new();
    send_order_ex(&mut conn, &mut context, &shared_for_test(), "DU123456", 7, 0, Side::Buy, 1, kind, tif, &attrs)
        .unwrap();
    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    String::from_utf8_lossy(&buf[..n]).to_string()
}

/// A short sale states that side, distinctly from a plain sale.
///
/// The venue refuses it — "sell short variant is not supported" — so no
/// live phase can show the side is written correctly, and a caller shorting
/// through this client depends on it being right the day a venue takes it.
#[test]
fn a_short_sale_states_its_own_side() {
    use std::io::Read;
    let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut context = Context::new();
    send_order_ex(
        &mut conn, &mut context, &shared_for_test(), "DU123456", 7, 0, Side::ShortSell, 1,
        crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
        b'1', &crate::types::OrderAttrs::default(),
    ).unwrap();
    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("54=").as_deref(), Some("5"), "a short sale, not a sale: {msg}");
}

/// What a volatility order does as the underlying moves.
///
/// A caller could state that the venue should keep re-pricing the order,
/// which price to reference, and the band of underlying prices to stay
/// inside — and the API accepted all four and sent none of them. An order
/// asking to be managed arrived asking for nothing of the sort.
#[test]
fn a_volatility_order_carries_what_it_asked_to_be_managed_by() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
        b'0',
        crate::types::OrderAttrs {
            volatility: 0.25,
            volatility_type: 2,
            continuous_update: true,
            reference_price_type: 2,
            stock_range_lower: 100.0,
            stock_range_upper: 200.0,
            ..Default::default()
        },
    );
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("6280=").as_deref(), Some("2"), "the volatility kind: {msg}");
    assert_eq!(tag("6275=").as_deref(), Some("1"), "kept re-priced: {msg}");
    assert_eq!(tag("6279=").as_deref(), Some("2"), "the price it references: {msg}");
    assert!(
        tag("6152=").is_some_and(|v| v.starts_with("100.")),
        "the band it stays above: {msg}",
    );
    assert!(
        tag("6153=").is_some_and(|v| v.starts_with("200.")),
        "the band it stays below: {msg}",
    );
}

/// An order that asked the venue to manage its price, to run for a set
/// time, and what to compete against.
///
/// All four are on the order this API takes and on the Python one, where a
/// caller coming from the reference client puts them, and none of them
/// reached the wire.
#[test]
fn an_order_carries_what_it_competes_against_and_how_long_it_runs() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
        b'0',
        crate::types::OrderAttrs {
            use_price_mgmt_algo: 1,
            duration: 60,
            min_compete_size: 100,
            compete_against_best_offset: 0.02,
            ..Default::default()
        },
    );
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("8339=").as_deref(), Some("1"), "price managed by the venue: {msg}");
    assert_eq!(tag("8402=").as_deref(), Some("60"), "how long it runs: {msg}");
    assert_eq!(tag("8411=").as_deref(), Some("100"), "the smallest size worth competing for: {msg}");
    assert!(
        tag("8412=").is_some_and(|v| v.starts_with("0.02")),
        "how far past the best price: {msg}",
    );
}

/// A default order states none of them, so an order that asked for nothing
/// does not arrive asking for something.
#[test]
fn a_default_order_competes_for_nothing() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
        b'0',
        crate::types::OrderAttrs::default(),
    );
    for t in ["8339=", "8402=", "8411=", "8412="] {
        assert!(!msg.contains(t), "{t} stated on an order that asked for nothing: {msg}");
    }
}

/// A midpoint peg whose offset is stated as two parts is the other form of
/// the order, and says so by its type rather than by an instruction.
#[test]
fn a_two_part_midpoint_offset_is_the_other_peg() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::PegMid { offset: crate::types::PRICE_SCALE / 100, price_cap: 0 },
        b'0',
        crate::types::OrderAttrs {
            mid_offset_at_whole: 0.01,
            mid_offset_at_half: 0.005,
            ..Default::default()
        },
    );
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("40=").as_deref(), Some("PMID2"), "the two-part peg: {msg}");
    assert!(tag("18=").is_none(), "the type carries it, not an instruction: {msg}");
    assert!(tag("8403=").is_some_and(|v| v.starts_with("0.01")), "the whole part: {msg}");
    assert!(tag("8404=").is_some_and(|v| v.starts_with("0.005")), "the half part: {msg}");
}

/// One part alone is not the two-part form, and the ordinary peg still
/// states its instruction.
#[test]
fn one_part_alone_is_still_the_ordinary_midpoint_peg() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::PegMid { offset: crate::types::PRICE_SCALE / 100, price_cap: 0 },
        b'0',
        crate::types::OrderAttrs { mid_offset_at_whole: 0.01, ..Default::default() },
    );
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("40=").as_deref(), Some("P"), "still the ordinary peg: {msg}");
    assert_eq!(tag("18=").as_deref(), Some("M"), "which states its instruction: {msg}");
}

/// A fill-or-kill order states that time in force on the wire.
///
/// This venue refuses the order for the security types the live suite can
/// reach — "the time-in-force FOK is invalid for this combination of
/// exchange and security type", on the default destination and on ISLAND
/// alike — so no live phase can show the encoding is right. What the venue
/// accepts is its own; what this client writes is not, and it is checked
/// here on the bytes.
#[test]
fn a_fill_or_kill_order_states_its_time_in_force() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
        b'4',
        crate::types::OrderAttrs::default(),
    );
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("59=").as_deref(), Some("4"), "fill or kill on the wire: {msg}");
}

/// An iceberg states how much of it is shown.
///
/// Refused live as well — "iceberg orders not supported for this
/// combination of exchange and security type" — and refused for every
/// displayed quantity tried, so the field never reaches a venue that would
/// act on it. It is still this client's job to write it.
#[test]
fn an_iceberg_order_states_the_quantity_it_shows() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
        b'1',
        crate::types::OrderAttrs { display_size: 100, ..Default::default() },
    );
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("111=").as_deref(), Some("100"), "the displayed quantity: {msg}");
}

/// The tags a bracket child cannot ship without. Asserted on the bytes
/// `send_order_ex` puts on the wire rather than on the request enum, which
/// carries them whether or not the encoder emits them.
#[test]
fn adjustable_stop_wire_carries_parent_oca_and_tif() {
    use std::io::Read;
    let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut context = Context::new();
    let attrs = crate::types::OrderAttrs {
        parent_id: 42,
        oca_group_str: "bracket_1".to_string(),
        oca_type: 1,
        ..Default::default()
    };
    send_order_ex(
        &mut conn,
        &mut context,
        &shared_for_test(),
        "DU123456",
        7,
        0,
        Side::Sell,
        1,
        crate::types::OrderKind::AdjustableStop {
            stop_price: 11 * crate::types::PRICE_SCALE,
            trigger_price: 12 * crate::types::PRICE_SCALE,
            adjusted_order_type: crate::types::AdjustedOrderType::Stop,
            adjusted_stop_price: 11 * crate::types::PRICE_SCALE + crate::types::PRICE_SCALE / 2,
            adjusted_stop_limit_price: 0,
            adjusted_trailing_amount: 0,
            adjustable_trailing_unit: 0,
        },
        b'1', // GTC
        &attrs,
    )
    .unwrap();

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]);
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

    assert_eq!(tag("6107=").as_deref(), Some("42.0"), "parent link missing: {msg}");
    assert_eq!(tag("583=").as_deref(), Some("bracket_1"), "OCA group missing: {msg}");
    assert_eq!(tag("59=").as_deref(), Some("1"), "tif must be GTC, not DAY: {msg}");
    // The adjustable-specific tags keep both the values and the position the
    // standalone arm gave them — after 204 and the attribute block — which
    // the sibling test pins by asserting 204 precedes 6257.
    assert_eq!(tag("40=").as_deref(), Some("3"));
    assert_eq!(tag("99="), Some(format_price(11 * crate::types::PRICE_SCALE).to_string()));
    assert_eq!(tag("6257=").as_deref(), Some("1"));
    assert_eq!(tag("6261=").as_deref(), Some(crate::types::AdjustedOrderType::Stop.fix_code()));
    assert_eq!(tag("6258="), Some(format_price(12 * crate::types::PRICE_SCALE).to_string()));
    assert_eq!(
        tag("6259="),
        Some(
            format_price(11 * crate::types::PRICE_SCALE + crate::types::PRICE_SCALE / 2)
                .to_string()
        )
    );
}

/// Tag 15 carries the currency the contract was registered with.
///
/// A constant here names the right currency for a US instrument and the wrong
/// one for every other, and an order naming the wrong currency names a
/// different contract. Where the caller states none, the tag is empty: the
/// venue infers the currency from the contract id.
#[test]
fn an_order_states_the_currency_the_contract_is_priced_in() {
    use std::io::Read;
    let sent = |key: Option<&str>| {
        let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut context = Context::new();
        let id = context.market.try_register_contract(0, "BMW", "STK", "IBIS", "").unwrap();
        context.set_symbol(id, "BMW".to_string());
        if let Some(k) = key {
            context.set_order_identity(id, k);
        }
        send_order_ex(
            &mut conn,
            &mut context,
            &shared_for_test(),
            "DU123456",
            12,
            id,
            Side::Buy,
            1,
            crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
            b'0',
            &crate::types::OrderAttrs::default(),
        )
        .unwrap();
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]).to_string();
        msg.split('\u{1}').find_map(|f| f.strip_prefix("15=").map(str::to_string)).unwrap()
    };

    assert_eq!(sent(Some("|0|||||EUR")), "EUR", "what the caller said");
    assert_eq!(sent(None), "", "and nothing where the caller stated nothing");
}

/// A contract named by conId alone carries no currency in its registration.
/// Tag 15 comes from the venue's definition of that contract instead of
/// defaulting to USD.
#[test]
fn an_order_falls_back_to_the_currency_the_venue_states() {
    use std::io::Read;
    let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut context = Context::new();
    let id = context.register_instrument(12087792);
    context.set_symbol(id, "EUR".to_string());
    let shared = shared_for_test();
    shared.reference.cache_contract(12087792, crate::types::model::Contract {
        con_id: 12087792,
        symbol: "EUR".into(),
        sec_type: "CASH".into(),
        currency: "GBP".into(),
        ..Default::default()
    });
    send_order_ex(
        &mut conn, &mut context, &shared, "DU123456", 13, id, Side::Buy, 1,
        crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
        b'0', &crate::types::OrderAttrs::default(),
    )
    .unwrap();
    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    let stated = msg.split('\u{1}').find_map(|f| f.strip_prefix("15=").map(str::to_string));
    assert_eq!(stated.as_deref(), Some("GBP"), "tag 15 from the definition: {msg}");
}

/// A preview states its price on the tag its own type carries it on. A stop
/// carries its price as the trigger on tag 99, not as a limit on tag 44.
#[test]
fn a_stop_preview_states_its_trigger_and_no_limit() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::Stop { stop_price: 90 * crate::types::PRICE_SCALE },
        b'0',
        crate::types::OrderAttrs { what_if: true, ..Default::default() },
    );
    assert!(msg.contains("\u{1}99=90\u{1}"), "the trigger is stated: {msg}");
    assert!(!msg.contains("\u{1}44="), "no limit price is stated: {msg}");
}

/// The trail percentage and the unit it is expressed in are different
/// fields, and a one percent trail is a hundred basis points while the code
/// for percent is also a hundred — so the two agree for exactly one
/// percentage and disagree for every other. Checked at two and a half.
#[test]
fn a_percent_trail_states_the_percent_and_the_unit_separately() {
    use std::io::Read;
    let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut context = Context::new();
    send_order_ex(
        &mut conn,
        &mut context,
        &shared_for_test(),
        "DU123456",
        9,
        0,
        Side::Sell,
        1,
        crate::types::OrderKind::TrailPct { trail_pct: 250, trail_stop_price: 0 },
        b'0',
        &crate::types::OrderAttrs::default(),
    )
    .unwrap();

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]);
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

    assert_eq!(tag("99=").as_deref(), Some("2.50"), "the percent, in decimal");
    assert_eq!(tag("211=").as_deref(), Some("2.50"), "and again where the peg carries it");
    assert_eq!(tag("6268=").as_deref(), Some("100"), "the unit is percent, not the percentage");
}

/// The conditional adjustable tags: 6262 only with a stop-limit conversion,
/// 6260/6269 only with a trailing one. Same rules as the standalone arm.
#[test]
fn adjustable_stop_wire_carries_trail_and_limit_tags() {
    use std::io::Read;
    let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut context = Context::new();
    send_order_ex(
        &mut conn,
        &mut context,
        &shared_for_test(),
        "DU123456",
        8,
        0,
        Side::Sell,
        1,
        crate::types::OrderKind::AdjustableStop {
            stop_price: 11 * crate::types::PRICE_SCALE,
            trigger_price: 12 * crate::types::PRICE_SCALE,
            adjusted_order_type: crate::types::AdjustedOrderType::TrailLimit,
            adjusted_stop_price: 11 * crate::types::PRICE_SCALE,
            adjusted_stop_limit_price: 10 * crate::types::PRICE_SCALE,
            adjusted_trailing_amount: crate::types::PRICE_SCALE / 2,
            adjustable_trailing_unit: 0,
        },
        b'0',
        &crate::types::OrderAttrs::default(),
    )
    .unwrap();

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]);
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

    assert_eq!(tag("6262="), Some(format_price(10 * crate::types::PRICE_SCALE).to_string()));
    assert_eq!(tag("6260="), Some(format_price(crate::types::PRICE_SCALE / 2).to_string()));
    assert_eq!(tag("6269=").as_deref(), Some("0"));
    // No parent, no OCA set: those tags must be absent, not empty.
    assert_eq!(tag("6107="), None);
    assert_eq!(tag("583="), None);

    // Order, not just presence: the adjustable tags sit after 204 and the
    // base type tags before 59, exactly where the dedicated encoder this
    // path replaced put them. Tag order is not supposed to carry meaning,
    // but this path had a shipped layout and there is no reason to change
    // it as a side effect.
    let pos = |t: &str| msg.split('\u{1}').position(|f| f.starts_with(t));
    assert!(pos("40=") < pos("59="), "base type tags precede tif: {msg}");
    assert!(pos("99=") < pos("59="), "stop price precedes tif: {msg}");
    assert!(pos("204=") < pos("6257="), "adjustable tags follow 204: {msg}");
    assert!(pos("6257=") < pos("6261="), "adjustable tags keep their order: {msg}");
    assert!(pos("6259=") < pos("6262="), "adjustable tags keep their order: {msg}");
    assert!(pos("6262=") < pos("6260="), "adjustable tags keep their order: {msg}");
}
mod modify_wire_tests {
    use super::super::*;
    use crate::protocol::connection::Connection;
    use std::io::Read;

    /// Drive the order queue and read what actually reaches the socket.
    fn drain(context: &mut Context) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut conn = Some(Connection::new_raw(client).unwrap());

        let shared = std::sync::Arc::new(SharedState::new());
        let mut hb = HeartbeatState::new();
        drain_and_send_orders(
            &mut conn, context, "DU111111", &mut hb, false, &shared, false, &None,
        );

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|")
    }

    /// A replace of an adjustable stop restates the conversion it carries.
    ///
    /// The order is an ordinary stop defined by what it becomes, and a replace
    /// that states only the type leaves that behind. It was refused here for
    /// that reason. The replace does carry it: the marker, the type it adjusts
    /// to, the trigger, the adjusted stop and the trailing amount, the same
    /// numbers the placement wrote.
    #[test]
    fn a_replace_restates_what_an_adjustable_stop_adjusts_to() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.insert_order(crate::types::Order::new(
            7, instrument, Side::Sell, crate::types::QTY_SCALE,
            0, b'3', b'0', 11 * crate::types::PRICE_SCALE,
        ));
        context.submitted.insert(7, Box::new(crate::types::OrderSpec {
            kind: crate::types::OrderKind::AdjustableStop {
                stop_price: 11 * crate::types::PRICE_SCALE,
                trigger_price: 12 * crate::types::PRICE_SCALE,
                adjusted_order_type: crate::types::AdjustedOrderType::Trail,
                adjusted_stop_price: 115 * crate::types::PRICE_SCALE / 10,
                adjusted_stop_limit_price: 0,
                adjusted_trailing_amount: crate::types::PRICE_SCALE / 2,
                adjustable_trailing_unit: 0,
            },
            attrs: crate::types::OrderAttrs::default(),
        }));
        context.modify(7, 105 * crate::types::PRICE_SCALE / 10, 1, false);
        let wire = drain(&mut context);

        assert!(wire.contains("|6257=1|"), "the marker: {wire}");
        assert!(wire.contains("|6261="), "what it adjusts to: {wire}");
        assert!(wire.contains("|6258=12|"), "the trigger: {wire}");
        assert!(wire.contains("|6259=11.5|"), "the stop it takes on: {wire}");
        assert!(wire.contains("|6260=0.5|"), "and the trailing amount: {wire}");
    }

    /// A plain limit modified with the flag in each polarity.
    fn replace_bytes(outside_rth: bool) -> String {
        let mut context = Context::new();
        context.insert_order(crate::types::Order::new(
            7,
            0,
            Side::Buy,
            crate::types::QTY_SCALE,
            100 * crate::types::PRICE_SCALE,
            b'2',
            b'0',
            0,
        ));
        context.modify(7, 200 * crate::types::PRICE_SCALE, 50, outside_rth);
        drain(&mut context)
    }

    /// The replace states 6433 as the caller set it, so an RTH-only order is
    /// not opted into the extended session by its first modify. Pins the flag's
    /// position — straight after 6122, ahead of where the order is working — in
    /// both polarities.
    #[test]
    fn modify_emits_outside_rth_only_when_the_caller_set_it() {
        let on = replace_bytes(true);
        assert!(
            on.contains("|6122=c|6433=1|100="),
            "6433 must keep its captured position after 6122: {on}"
        );

        let off = replace_bytes(false);
        assert!(!off.contains("|6433="), "an RTH-only order must not assert 6433: {off}");
        assert!(off.contains("|6122=c|100="), "the rest of the message is unchanged: {off}");
    }

    /// A stop has no limit leg, so the price a caller supplies to a modify can
    /// only mean the trigger. Writing it to tag 44 and restating the original
    /// trigger in 99 leaves the stop where it was, and the venue rejects the
    /// replace outright, so the order the caller meant to move ends Inactive.
    #[test]
    fn modifying_a_stop_moves_its_trigger() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.insert_order(crate::types::Order::new(
            7,
            instrument,
            Side::Sell,
            crate::types::QTY_SCALE,
            600 * crate::types::PRICE_SCALE,
            b'3',
            b'0',
            600 * crate::types::PRICE_SCALE,
        ));

        context.modify(7, 610 * crate::types::PRICE_SCALE, 1, false);
        let sent = drain(&mut context);

        assert!(sent.contains("|99=610|"), "the trigger moves to the new price: {sent}");
        assert!(!sent.contains("|99=600|"), "and does not restate the old one: {sent}");
        assert!(!sent.contains("|44="), "a stop has no limit leg to state: {sent}");
    }

    /// Every trigger-only type, not just the plain stop. Market-if-touched and
    /// stop-with-protection have no limit leg either.
    #[test]
    fn every_trigger_only_type_moves_its_trigger() {
        for (ord_type, name) in
            [(b'3', "STP"), (b'J', "MIT"), (crate::types::ORD_STP_PRT, "STP PRT")]
        {
            let mut context = Context::new();
            let instrument = context.register_instrument(756733);
            context.insert_order(crate::types::Order::new(
                7,
                instrument,
                Side::Sell,
                crate::types::QTY_SCALE,
                600 * crate::types::PRICE_SCALE,
                ord_type,
                b'0',
                600 * crate::types::PRICE_SCALE,
            ));

            context.modify(7, 610 * crate::types::PRICE_SCALE, 1, false);
            let sent = drain(&mut context);

            assert!(sent.contains("|99=610|"), "{name}: trigger moves: {sent}");
            assert!(!sent.contains("|44="), "{name}: no limit leg to state: {sent}");
        }

        // The bucket is pinned from above as well: a type with a limit leg
        // keeps it. A market or market-to-limit order has none, and stating
        // one is refused — "Invalid value in field # 44".
        for ord_type in *b"UK1" {
            let mut context = Context::new();
            let instrument = context.register_instrument(756733);
            context.insert_order(crate::types::Order::new(
                7,
                instrument,
                Side::Sell,
                crate::types::QTY_SCALE,
                100 * crate::types::PRICE_SCALE,
                ord_type,
                b'0',
                0,
            ));
            context.modify(7, 610 * crate::types::PRICE_SCALE, 1, false);
            let sent = drain(&mut context);
            assert!(
                !sent.contains("|44="),
                "{ord_type} has no limit leg to state: {sent}",
            );
        }

        for ord_type in *b"2" {
            let mut context = Context::new();
            let instrument = context.register_instrument(756733);
            context.insert_order(crate::types::Order::new(
                7,
                instrument,
                Side::Sell,
                crate::types::QTY_SCALE,
                100 * crate::types::PRICE_SCALE,
                ord_type,
                b'0',
                0,
            ));
            context.modify(7, 610 * crate::types::PRICE_SCALE, 1, false);
            let sent = drain(&mut context);
            assert!(
                sent.contains("|44=610|"),
                "{ord_type} is not trigger-only and keeps tag 44: {sent}",
            );
        }
    }

    /// A replace restates the shape the order was placed with, and a peg to
    /// benchmark was placed with no tag 44. The price a caller names on the
    /// request has no field of its own on this type, so it is not stated —
    /// stating a limit the submit never sent describes a different order.
    #[test]
    fn a_benchmark_peg_is_replaced_without_a_price_on_tag_44() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.market.set_routing(instrument, "STK", "SMART");
        context.pending_orders.push(crate::types::OrderRequest::SubmitEx {
            order_id: 7,
            instrument,
            con_id: 0,
            side: Side::Buy,
            qty: crate::types::QTY_SCALE,
            kind: crate::types::OrderKind::PegBench {
                price: 150 * crate::types::PRICE_SCALE,
                ref_con_id: 756733,
                is_peg_decrease: false,
                pegged_change_amount: crate::types::PRICE_SCALE,
                ref_change_amount: crate::types::PRICE_SCALE,
                starting_price: 149 * crate::types::PRICE_SCALE,
                stock_ref_price: 149 * crate::types::PRICE_SCALE,
                ref_exchange: "SMART".into(),
            },
            tif: b'0',
            attrs: crate::types::OrderAttrs::default(),
        });
        let placed = drain(&mut context);
        assert!(!placed.contains("|44="), "the submit states no price on tag 44: {placed}");

        context.pending_orders.push(crate::types::OrderRequest::Modify {
            order_id: 7, ord_type: 0, tif: 0, price: 151 * crate::types::PRICE_SCALE,
            qty: 2 * crate::types::QTY_SCALE, outside_rth: false, stop_price: 0,
            spec: None,
        });
        let sent = drain(&mut context);
        assert!(sent.contains("|40=PB|"), "it is still a benchmark peg: {sent}");
        assert!(!sent.contains("|44="), "and the replace states no price on tag 44: {sent}");
    }

    /// The other side of the same rule: a replace that states both the type
    /// and the trigger is stating a real one. Deciding from the resting order
    /// alone sent a stop-limit with no tag 99, which is not a stop-limit.
    #[test]
    fn a_replace_into_a_stop_limit_states_the_trigger_it_was_given() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        // A plain limit, so there is no resting trigger to fall back on.
        context.insert_order(crate::types::Order::new(
            7,
            instrument,
            Side::Sell,
            crate::types::QTY_SCALE,
            100 * crate::types::PRICE_SCALE,
            b'2',
            b'0',
            0,
        ));
        context.pending_orders.push(crate::types::OrderRequest::Modify {
            order_id: 7,
            ord_type: b'4',
            tif: 0,
            price: 101 * crate::types::PRICE_SCALE,
            qty: crate::types::QTY_SCALE,
            outside_rth: false,
            stop_price: 99 * crate::types::PRICE_SCALE,
            spec: None,
        });
        let sent = drain(&mut context);

        assert!(sent.contains("|40=4|"), "it is a stop-limit now: {sent}");
        assert!(sent.contains("|44=101|"), "with its limit leg: {sent}");
        assert!(sent.contains("|99=99|"), "and the trigger it was given: {sent}");
    }

    /// A type that carries no trigger must not acquire one. The public client
    /// fills the request's trigger from `aux_price`, which is meaningless on a
    /// limit and is the offset on a pegged order — neither belongs in tag 99.
    #[test]
    fn a_type_without_a_trigger_never_gains_one() {
        // Whether the type states a limit leg at all: the venue refuses one
        // on a type whose submit states none — "Invalid value in field # 44".
        // No type defined by an offset is here: replacing one restates that
        // offset from the record of the order as it was placed, and one
        // inserted rather than placed has none — see the test below.
        for (ord_type, name, has_a_limit_leg) in [
            (b'2', "LMT", true),
            (b'1', "MKT", false),
            (b'K', "MTL", false),
            (b'5', "MOC", false),
        ] {
            let mut context = Context::new();
            let instrument = context.register_instrument(756733);
            // Tracked with no trigger. A pegged or relative order tracks its
            // offset in this field, so this pins the request-supplied path
            // rather than claiming those types never emit a 99.
            context.insert_order(crate::types::Order::new(
                7,
                instrument,
                Side::Sell,
                crate::types::QTY_SCALE,
                100 * crate::types::PRICE_SCALE,
                ord_type,
                b'0',
                0,
            ));

            // A trigger arrives on the request anyway.
            context.pending_orders.push(crate::types::OrderRequest::Modify {
                order_id: 7,
                ord_type: 0,
                tif: 0,
                price: 101 * crate::types::PRICE_SCALE,
                qty: crate::types::QTY_SCALE,
                outside_rth: false,
                stop_price: 610 * crate::types::PRICE_SCALE,
                spec: None,
            });
            let sent = drain(&mut context);

            assert!(!sent.contains("|99="), "{name} must not gain a trigger: {sent}");
            if has_a_limit_leg {
                assert!(sent.contains("|44=101|"), "{name} keeps its limit leg: {sent}");
            } else {
                assert!(!sent.contains("|44="), "{name} has no limit leg to state: {sent}");
            }
        }
    }

    /// A converted order does not get the type it left restated onto it.
    ///
    /// The record is the order as it was placed and a replace never rewrites
    /// it. Compared against the resting order rather than the record, the
    /// second replace after a conversion looks like it keeps the type — while
    /// the record still describes the type before the conversion, whose prices
    /// would then go out under the new type's name.
    #[test]
    fn a_second_replace_after_a_conversion_states_no_stale_price() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.market.set_routing(instrument, "STK", "SMART");
        // Placed as a limit, so the record describes a limit at 100.
        context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
            order_id: 7,
            instrument,
            side: Side::Buy,
            qty: crate::types::QTY_SCALE,
            kind: crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
            tif: b'0',
            attrs: crate::types::OrderAttrs::default(),
        });
        let _ = drain(&mut context);

        // Converted to a market order, then replaced again as a market order.
        context.pending_orders.push(crate::types::OrderRequest::Modify {
            order_id: 7, ord_type: b'1', tif: 0, price: 0,
            qty: crate::types::QTY_SCALE, outside_rth: false, stop_price: 0,
            spec: None,
        });
        let _ = drain(&mut context);
        context.pending_orders.push(crate::types::OrderRequest::Modify {
            order_id: 7, ord_type: b'1', tif: 0, price: 0,
            qty: 2 * crate::types::QTY_SCALE, outside_rth: false, stop_price: 0,
            spec: None,
        });
        let sent = drain(&mut context);

        assert!(sent.contains("|40=1|"), "it is a market order: {sent}");
        assert!(!sent.contains("|44="), "with no price the limit left behind: {sent}");
    }

    /// A conversion carries none of the old type's instructions either.
    ///
    /// The execution instruction, the peg offset and the attributes are the
    /// record's as much as its prices are. A trailing stop converted to a
    /// market order that still stated `18=a` would be asking for a trailing
    /// market order, which is not what the caller asked for.
    #[test]
    fn a_conversion_states_none_of_the_old_type_s_instructions() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.market.set_routing(instrument, "STK", "SMART");
        context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
            order_id: 7,
            instrument,
            side: Side::Sell,
            qty: crate::types::QTY_SCALE,
            kind: crate::types::OrderKind::TrailingStop {
                trail_stop_price: 0,
                trail_amt: 5 * crate::types::PRICE_SCALE,
            },
            tif: b'0',
            attrs: crate::types::OrderAttrs::default(),
        });
        let _ = drain(&mut context);

        context.pending_orders.push(crate::types::OrderRequest::Modify {
            order_id: 7, ord_type: b'1', tif: 0, price: 0,
            qty: crate::types::QTY_SCALE, outside_rth: false, stop_price: 0,
            spec: None,
        });
        let sent = drain(&mut context);

        assert!(sent.contains("|40=1|"), "it is a market order: {sent}");
        assert!(!sent.contains("|18="), "with no trailing instruction: {sent}");
        assert!(!sent.contains("|211="), "and no trail: {sent}");
    }

    /// A reconnect does not cost a pegged order its peg.
    ///
    /// The venue names a pegged order back under `P` and the replay records
    /// that, while the record it was placed under still holds the discriminant
    /// this client tells the two pegs apart by. Compared as bytes those are
    /// different types and the replace states none of the peg; compared as the
    /// venue names them they are one, which is what they are.
    #[test]
    fn a_replay_does_not_cost_a_pegged_order_its_peg() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.market.set_routing(instrument, "STK", "SMART");
        context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
            order_id: 7,
            instrument,
            side: Side::Buy,
            qty: crate::types::QTY_SCALE,
            kind: crate::types::OrderKind::PegMkt { offset: crate::types::PRICE_SCALE, price_cap: 0 },
            tif: b'0',
            attrs: crate::types::OrderAttrs::default(),
        });
        let _ = drain(&mut context);

        // The venue replays it under the name it uses on the wire.
        context.insert_order(crate::types::Order::new(
            7, instrument, Side::Buy, crate::types::QTY_SCALE, 0, b'P', b'0', 0,
        ));

        context.pending_orders.push(crate::types::OrderRequest::Modify {
            order_id: 7, ord_type: 0, tif: 0, price: 0,
            qty: 2 * crate::types::QTY_SCALE, outside_rth: false, stop_price: 0,
            spec: None,
        });
        let sent = drain(&mut context);
        assert!(sent.contains("|211="), "the peg's offset is restated: {sent}");
    }

    /// No order defined by an offset is replaced without its placed record.
    ///
    /// A trail, a peg offset and a limit-versus-trail offset all live in the
    /// record rather than in the replace, so an order the venue replayed at
    /// connect has nothing to restate them from. Sent anyway the replace is
    /// refused naming the field it left out.
    #[test]
    fn no_offset_order_is_replaced_without_its_record() {
        for (name, ord_type) in [
            ("TRAIL", b'P'),
            ("TSL", crate::types::ORD_TRAIL_LIMIT),
            ("PEG MKT", crate::types::ORD_PEG_MKT),
            ("PEG MID", crate::types::ORD_PEG_MID),
            ("PB", crate::types::ORD_PEG_BENCH),
            ("SMKT", crate::types::ORD_SNAP_MKT),
            ("SMID", crate::types::ORD_SNAP_MID),
            ("SREL", crate::types::ORD_SNAP_PRI),
        ] {
            let mut context = Context::new();
            let instrument = context.register_instrument(756733);
            context.insert_order(crate::types::Order::new(
                7, instrument, Side::Sell, crate::types::QTY_SCALE, 0, ord_type, b'0', 0,
            ));
            context.modify(7, 0, 2, false);

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let client = std::net::TcpStream::connect(addr).unwrap();
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(std::time::Duration::from_millis(100))).unwrap();
            let mut conn = Some(Connection::new_raw(client).unwrap());
            let shared = std::sync::Arc::new(SharedState::new());
            let mut hb = HeartbeatState::new();
            drain_and_send_orders(
                &mut conn, &mut context, "DU111111", &mut hb, false, &shared, false, &None,
            );

            let mut buf = [0u8; 4096];
            assert!(peer.read(&mut buf).is_err(), "{name} reaches the venue");
            let told = shared.orders.drain_order_inactive();
            assert!(
                told.iter().any(|(id, _, why)| *id == 7 && why.contains("cannot be restated")),
                "{name}: the caller is told why: {told:?}",
            );
        }
    }

    /// A trailing stop this session did not place cannot be replaced by it.
    ///
    /// The trail rides on tag 211 and is restated from the record of the order
    /// as it was placed. An order the venue replayed at connect has no such
    /// record, so the replace would go out without the field that defines it
    /// and be refused naming it. The caller is told here instead.
    #[test]
    fn a_trailing_stop_with_no_placed_record_is_not_replaced() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.insert_order(crate::types::Order::new(
            7,
            instrument,
            Side::Sell,
            crate::types::QTY_SCALE,
            0,
            b'P',
            b'0',
            0,
        ));
        context.modify(7, 0, 2, false);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
        let mut conn = Some(Connection::new_raw(client).unwrap());
        let shared = std::sync::Arc::new(SharedState::new());
        let mut hb = HeartbeatState::new();
        drain_and_send_orders(
            &mut conn, &mut context, "DU111111", &mut hb, false, &shared, false, &None,
        );

        let mut buf = [0u8; 4096];
        assert!(peer.read(&mut buf).is_err(), "nothing reaches the venue");
        let told = shared.orders.drain_order_inactive();
        assert!(
            told.iter().any(|(id, _, why)| *id == 7 && why.contains("cannot be restated")),
            "and the caller is told why: {told:?}",
        );
    }

    /// A two-legged type can have its trigger moved when it has one.
    /// Pegged-to-market and pegged-to-midpoint share OrdType "E" and are told
    /// apart by ExecInst, exactly as `ORD_PEG_MKT` and `ORD_PEG_MID` document
    /// in types.rs. Neither emitted tag 18, so the two went on the wire byte
    /// for byte identical and neither said which peg it was — every other
    /// shared-OrdType pair in this encoder does emit its disambiguator.
    /// An option order that does not restate expiry, strike and right names no
    /// particular contract: the symbol alone is the whole chain. That is why
    /// non-stock orders were refused rather than sent, and carrying the identity
    /// is what makes sending one safe.
    #[test]
    fn an_option_order_names_its_contract() {
        let mut context = Context::new();
        let instrument = context
            .market
            .try_register_contract(0, "AAPL", "OPT", "SMART", "20260619|230|C|100")
            .expect("slot");
        context.market.set_symbol(instrument, "AAPL".into());
        context.market.set_routing(instrument, "OPT", "SMART");

        context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
            order_id: 7,
            instrument,
            side: Side::Buy,
            qty: crate::types::QTY_SCALE,
            kind: crate::types::OrderKind::Limit { price: 5 * crate::types::PRICE_SCALE },
            tif: b'0',
            attrs: crate::types::OrderAttrs::default(),
        });
        let sent = drain(&mut context);

        assert!(sent.contains("|167=OPT|"), "the security type: {sent}");
        assert!(sent.contains("|541=20260619|"), "a full date on the maturity-date tag: {sent}");
        assert!(sent.contains("|202=230|"), "the strike: {sent}");
        assert!(sent.contains("|201=1|"), "the right, as the wire code for a call: {sent}");
        assert!(sent.contains("|231=100|"), "the multiplier: {sent}");
    }

    /// The caller's decimal reaches the wire. Tag 38 written from an integer
    /// sent `38=0` for half a share, which asks the venue for nothing.
    #[test]
    fn a_fractional_quantity_goes_out_as_the_decimal_it_was_given() {
        let mut context = Context::new();
        let instrument = context
            .market
            .try_register_contract(0, "AAPL", "STK", "SMART", "")
            .expect("slot");
        context.market.set_symbol(instrument, "AAPL".into());
        context.market.set_routing(instrument, "STK", "SMART");

        context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
            order_id: 9,
            instrument,
            side: Side::Buy,
            qty: crate::types::QTY_SCALE / 2,
            kind: crate::types::OrderKind::Limit { price: 150 * crate::types::PRICE_SCALE },
            tif: b'0',
            attrs: Default::default(),
        });
        let sent = drain(&mut context);

        assert!(sent.contains("|38=0.5|"), "half a share is stated as half a share: {sent}");
    }

    /// An exercise and a lapse are new orders carrying the action, and nothing
    /// else tells them apart from each other or from an ordinary order. Read
    /// off the socket, because the request that carries the action and the
    /// message that states it are two different things.
    #[test]
    fn an_exercise_and_a_lapse_go_out_as_new_orders_carrying_the_action() {
        for (action, stated) in [(1u8, "1"), (2, "2")] {
            let mut context = Context::new();
            let instrument = context
                .market
                .try_register_contract(0, "AAPL", "OPT", "SMART", "20260619|230|C|100")
                .expect("slot");
            context.market.set_symbol(instrument, "AAPL".into());
            context.market.set_routing(instrument, "OPT", "SMART");

            context.pending_orders.push(
                crate::client_core::ClientCore::build_exercise_request(
                    7, instrument, action, 3 * crate::types::QTY_SCALE, Default::default()),
            );
            let sent = drain(&mut context);

            assert!(sent.contains("|35=D|"), "an exercise is a new order: {sent}");
            assert!(
                sent.contains(&format!("|6809={stated}|")),
                "carrying the action it was asked for: {sent}",
            );
            assert!(sent.contains("|38=3|"), "for the contracts named: {sent}");
            assert!(sent.contains("|54=1|"), "on the buy side: {sent}");
            assert!(sent.contains("|541=20260619|"), "and naming the option: {sent}");
        }
    }

    /// A stock names itself with its symbol, so none of those tags belong on
    /// it. A contract known by conId still restates its identity on the wire: a
    /// future naming its exchange and not its month is not accepted.
    #[test]
    fn a_future_known_by_con_id_still_names_its_month() {
        let mut context = Context::new();
        let instrument = context
            .market
            .try_register_contract(793_356_217, "MES", "FUT", "CME", "202609|0||5")
            .expect("slot");
        context.market.set_symbol(instrument, "MES".into());
        context.market.set_routing(instrument, "FUT", "CME");

        context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
            order_id: 7,
            instrument,
            side: Side::Buy,
            qty: crate::types::QTY_SCALE,
            kind: crate::types::OrderKind::Limit { price: 3827 * crate::types::PRICE_SCALE },
            tif: b'0',
            attrs: crate::types::OrderAttrs::default(),
        });
        let sent = drain(&mut context);
        assert!(sent.contains("|167=FUT|"), "the security type: {sent}");
        assert!(sent.contains("|200=202609|"), "and the contract month: {sent}");
        assert!(sent.contains("|231=5|"), "and the multiplier: {sent}");
    }

    #[test]
    fn a_stock_order_carries_no_option_identity() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.market.set_symbol(instrument, "SPY".into());
        context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
            order_id: 7,
            instrument,
            side: Side::Buy,
            qty: crate::types::QTY_SCALE,
            kind: crate::types::OrderKind::Limit { price: 5 * crate::types::PRICE_SCALE },
            tif: b'0',
            attrs: crate::types::OrderAttrs::default(),
        });
        let sent = drain(&mut context);
        for tag in ["|200=", "|201=", "|202=", "|231="] {
            assert!(!sent.contains(tag), "a stock carries no {tag}: {sent}");
        }
    }

    #[test]
    fn the_two_pegs_are_told_apart_on_the_wire() {
        let mut sent = Vec::new();
        for kind in [
            crate::types::OrderKind::PegMkt { offset: 5 * crate::types::PRICE_SCALE, price_cap: 0 },
            crate::types::OrderKind::PegMid { offset: 5 * crate::types::PRICE_SCALE, price_cap: 0 },
        ] {
            let mut context = Context::new();
            let instrument = context.register_instrument(756733);
            context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
                order_id: 7,
                instrument,
                side: Side::Buy,
                qty: crate::types::QTY_SCALE,
                kind,
                tif: b'0',
                attrs: crate::types::OrderAttrs::default(),
            });
            sent.push(drain(&mut context));
        }
        // Asked live, the venue names these back as PegToMkt and PegToMid under
        // "P". Sent as "E" it named them something else entirely and refused
        // them under that other name, so a caller asking to peg had an order
        // the venue read as a different type — which is worse than a refusal.
        assert!(sent[0].contains("|40=P|"), "pegged to market is OrdType P: {}", sent[0]);
        assert!(sent[1].contains("|40=P|"), "pegged to midpoint is OrdType P: {}", sent[1]);
        assert!(sent[0].contains("|18=P|"), "pegged to market states its peg: {}", sent[0]);
        assert!(sent[1].contains("|18=M|"), "pegged to midpoint states its peg: {}", sent[1]);
        // The offset is stated once. Written twice the venue read the second.
        assert_eq!(sent[0].matches("|211=").count(), 1, "one offset: {}", sent[0]);
        assert_ne!(sent[0], sent[1], "the two pegs must not be the same message");
    }

    #[test]
    fn a_supplied_trigger_moves_a_two_legged_order() {
        for (ord_type, name, fix_type) in
            [(b'4', "STP LMT", "4"), (crate::types::ORD_LIT, "LIT", "LT")]
        {
            let mut context = Context::new();
            let instrument = context.register_instrument(756733);
            context.insert_order(crate::types::Order::new(
                7,
                instrument,
                Side::Sell,
                crate::types::QTY_SCALE,
                605 * crate::types::PRICE_SCALE,
                ord_type,
                b'0',
                600 * crate::types::PRICE_SCALE,
            ));

            context.pending_orders.push(crate::types::OrderRequest::Modify {
                order_id: 7,
                ord_type: 0,
                tif: 0,
                price: 610 * crate::types::PRICE_SCALE,
                qty: crate::types::QTY_SCALE,
                outside_rth: false,
                stop_price: 590 * crate::types::PRICE_SCALE,
                spec: None,
            });
            let sent = drain(&mut context);

            assert!(sent.contains("|44=610|"), "{name}: the limit moves: {sent}");
            assert!(sent.contains("|99=590|"), "{name}: and so does the trigger: {sent}");
            // The replace restates the tag 40 value the submit wrote.
            assert!(
                sent.contains(&format!("|40={fix_type}|")),
                "{name}: the replace restates OrdType {fix_type}: {sent}",
            );
        }
    }

    /// The replacement carries the trigger forward, so a second modify still
    /// has one to restate.
    #[test]
    fn the_replacement_keeps_the_trigger() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.insert_order(crate::types::Order::new(
            7,
            instrument,
            Side::Sell,
            crate::types::QTY_SCALE,
            600 * crate::types::PRICE_SCALE,
            b'3',
            b'0',
            600 * crate::types::PRICE_SCALE,
        ));

        let second = context.modify(7, 610 * crate::types::PRICE_SCALE, 1, false);
        drain(&mut context);
        assert_eq!(
            context.order(second).expect("tracked").stop_price,
            610 * crate::types::PRICE_SCALE,
            "the replacement records the trigger it just asked for",
        );

        context.modify(second, 620 * crate::types::PRICE_SCALE, 1, false);
        let sent = drain(&mut context);
        assert!(sent.contains("|99=620|"), "and the next modify moves it again: {sent}");
    }

    /// A type with both legs keeps the limit on 44 and holds its trigger.
    #[test]
    fn modifying_a_stop_limit_moves_the_limit_and_keeps_the_trigger() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.insert_order(crate::types::Order::new(
            7,
            instrument,
            Side::Sell,
            crate::types::QTY_SCALE,
            605 * crate::types::PRICE_SCALE,
            b'4',
            b'0',
            600 * crate::types::PRICE_SCALE,
        ));

        context.modify(7, 610 * crate::types::PRICE_SCALE, 1, false);
        let sent = drain(&mut context);

        assert!(sent.contains("|44=610|"), "the limit moves: {sent}");
        assert!(sent.contains("|99=600|"), "the trigger is restated unchanged: {sent}");
    }

    /// A cancel that has seen no echo yet computes `{id}.{ver}` for
    /// OrigClOrdID, so a bracket leg must be submitted under that same form: a
    /// bare ClOrdID names an id the venue is not holding once the leg is
    /// cancelled before its first execution report. Tag 6107 must also agree
    /// with the parent link `send_order_ex` puts on a child of the same order.
    #[test]
    fn a_bracket_leg_is_submitted_under_the_id_its_cancel_will_name() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        let (parent, tp, sl) = context.submit_bracket(
            instrument,
            Side::Buy,
            1,
            100 * crate::types::PRICE_SCALE,
            110 * crate::types::PRICE_SCALE,
            90 * crate::types::PRICE_SCALE,
        );
        let submitted = drain(&mut context);

        for id in [parent, tp, sl] {
            assert!(
                submitted.contains(&format!("|11={id}.0|")),
                "leg {id} is submitted versioned: {submitted}"
            );
        }
        assert_eq!(
            submitted.matches(&format!("|6107={parent}.0|")).count(),
            2,
            "both children link the parent by the id it was submitted under: {submitted}"
        );

        // Nothing has echoed, so the cancel computes the OrigClOrdID.
        context.cancel(tp);
        let cancelled = drain(&mut context);
        assert!(
            cancelled.contains(&format!("|41={tp}.0|")),
            "the cancel names the submitted id: {cancelled}"
        );
    }
}
mod outside_rth_polarity_tests {
    use super::super::*;
    use super::shared_for_test;
    use crate::protocol::connection::Connection;
    use std::io::Read;

    /// Drive the queued orders and read what actually reaches the socket.
    fn drain(context: &mut Context) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut conn = Some(Connection::new_raw(client).unwrap());

        let shared = std::sync::Arc::new(SharedState::new());
        let mut hb = HeartbeatState::new();
        drain_and_send_orders(
            &mut conn, context, "DU111111", &mut hb, false, &shared, false, &None,
        );

        let mut buf = vec![0u8; 8192];
        let n = peer.read(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|")
    }

    /// Every submit path guards tag 6433, and nothing asserted the guard: the
    /// assertions checked that the flag is present when the caller asked for
    /// it, never that it is absent when they did not. Making every encoder emit
    /// it unconditionally therefore failed no test.
    ///
    /// That is the shape took on the replace path, where a hard-coded
    /// 6433 opted every modified order into the extended session. An order
    /// widened to outside regular hours fills at prices the caller never meant
    /// to trade at, and no callback distinguishes it.
    /// A named submit path, invoked as (context, instrument, outside_rth).
    type SubmitCase = (&'static str, fn(&mut Context, u32, bool) -> crate::types::OrderId);

    #[test]
    fn every_submit_path_emits_outside_rth_only_when_it_was_asked_for() {
        let cases: Vec<SubmitCase> = vec![
            ("limit gtc", |c, i, o| {
                c.submit(i, Side::Buy, 1, crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE }, b'1', crate::types::OrderAttrs { outside_rth: o, ..Default::default() })
            }),
            ("stop gtc", |c, i, o| {
                c.submit(i, Side::Sell, 1, crate::types::OrderKind::Stop { stop_price: 90 * crate::types::PRICE_SCALE }, b'1', crate::types::OrderAttrs { outside_rth: o, ..Default::default() })
            }),
            ("stop limit gtc", |c, i, o| {
                c.submit(i, Side::Sell, 1, crate::types::OrderKind::StopLimit { price: 89 * crate::types::PRICE_SCALE, stop_price: 90 * crate::types::PRICE_SCALE }, b'1', crate::types::OrderAttrs { outside_rth: o, ..Default::default() })
            }),
            ("extended encoder", |c, i, o| {
                c.submit(i, Side::Buy, 1, crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE }, b'0',
                    crate::types::OrderAttrs { outside_rth: o, ..Default::default() })
            }),
        ];

        for (label, submit) in cases {
            for asked in [true, false] {
                let mut context = Context::new();
                let instrument = context.register_instrument(756733);
                submit(&mut context, instrument, asked);
                let sent = drain(&mut context);

                assert_eq!(
                    sent.contains("|6433=1|"),
                    asked,
                    "{label}, outside_rth={asked}: {sent}",
                );
            }
        }
    }

    /// A connection, a context and an instrument, for a combination order.
    fn combo_test_state() -> (
        crate::protocol::connection::Connection,
        std::net::TcpStream,
        Context,
        crate::types::InstrumentId,
    ) {
        let (conn, peer) = crate::protocol::connection::Connection::for_test();
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.set_symbol(instrument, "SPY".to_string());
        (conn, peer, context, instrument)
    }

    /// A combination names its legs on the order. There is no repeating group
    /// for them: a count, then a contract, a ratio and a side per leg. The side
    /// is a flag rather than the letter the order itself uses, and a leg that
    /// routes with the combination states no venue of its own.
    #[test]
    fn a_combination_names_each_of_its_legs() {
        use std::io::Read;
        let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.set_symbol(instrument, "SPY".to_string());
        let attrs = crate::types::OrderAttrs {
            combo_legs: vec![
                crate::types::ComboLegSpec {
                    con_id: 265598,
                    ratio: 1,
                    is_sell: false,
                    exchange: String::new(),
                    open_close: 1,
                    short_sale_slot: 0,
                    designated_location: String::new(),
                    exempt_code: -1,
                    price: None,
                },
                crate::types::ComboLegSpec {
                    con_id: 272093,
                    ratio: 2,
                    is_sell: true,
                    exchange: "ARCA".into(),
                    open_close: 0,
                    short_sale_slot: 0,
                    designated_location: String::new(),
                    exempt_code: -1,
                    price: None,
                },
            ],
            ..Default::default()
        };
        send_order_ex(
            &mut conn,
            &mut context,
            &shared_for_test(),
            "DU123456",
            31,
            instrument,
            Side::Buy,
            1,
            crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
            b'0',
            &attrs,
        )
        .unwrap();

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]);
        let f: Vec<&str> = msg.split('\u{1}').collect();
        assert!(f.contains(&"6079=2"), "the leg count: {msg}");
        assert!(f.contains(&"6080=265598") && f.contains(&"6080=272093"), "each contract: {msg}");
        assert!(f.contains(&"6081=1") && f.contains(&"6081=2"), "each ratio: {msg}");
        assert!(f.contains(&"6082=0") && f.contains(&"6082=1"), "each side, as a flag: {msg}");
        // The buying leg comes first, and it is the one carrying 1.
        let sides: Vec<&&str> = f.iter().filter(|t| t.starts_with("6082=")).collect();
        assert_eq!(*sides[0], "6082=1", "a bought leg: {msg}");
        assert_eq!(*sides[1], "6082=0", "a sold leg: {msg}");
        assert!(
            f.contains(&"616=") && f.contains(&"616=ARCA"),
            "a venue only where the leg has its own: {msg}"
        );
        assert!(f.contains(&"6087=1"), "the position effect where set: {msg}");
        // On every leg, including the one whose effect is nought: nought is a
        // stated effect there, not an absence, and a combination stating fewer
        // effects than it has legs leaves the venue one short of attaching one
        // to each.
        assert_eq!(
            f.iter().filter(|field| field.starts_with("6087=")).count(), 2,
            "one effect per leg: {msg}",
        );
        // And 654 keys the short-sale group rather than positioning a leg: one
        // row per leg, so what a locate belongs to is stated rather than
        // guessed from the order the fields arrive in.
        assert_eq!(
            f.iter().filter(|field| field.starts_with("654=")).count(), 2,
            "a short-sale row per leg: {msg}",
        );
    }

    /// A short leg states the borrow: which slot it comes from, the flag every
    /// short leg carries, where it is located and the exemption that applies.
    ///
    /// The flag went out on no leg at all. It is the same value every time, so
    /// nothing a caller stated was lost — but a short sale is the one thing on
    /// this message the venue is always told in full, and this leg was the
    /// exception.
    #[test]
    fn a_short_leg_states_the_borrow_in_full() {
        use std::io::Read;
        let (mut conn, mut peer, mut context, instrument) = combo_test_state();
        let attrs = crate::types::OrderAttrs {
            combo_legs: vec![crate::types::ComboLegSpec {
                con_id: 265598,
                ratio: 1,
                is_sell: true,
                exchange: String::new(),
                open_close: 2,
                short_sale_slot: 2,
                designated_location: "IBKR".into(),
                exempt_code: 3,
                price: None,
            }],
            ..Default::default()
        };
        send_order_ex(
            &mut conn, &mut context, &shared_for_test(), "DU123456", 32, instrument, Side::Sell,
            1, crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
            b'0', &attrs,
        )
        .unwrap();

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]);
        let f: Vec<&str> = msg.split('\u{1}').collect();
        // The group is keyed by leg, so what belongs to which leg is stated
        // rather than inferred from the order the fields happen to arrive in.
        assert!(f.contains(&"654=0"), "the leg the group is about: {msg}");
        assert!(f.contains(&"6215=N"), "the flag every short leg carries: {msg}");
        assert!(f.contains(&"6216=IBKR"), "where the borrow is: {msg}");
        assert!(f.contains(&"1689=3"), "the exemption: {msg}");
        assert!(f.contains(&"6087=2"), "and the leg closes rather than opens: {msg}");
        // Whether the combination is a short sale at all is stated once, and
        // only for the slot the venue reads on that tag. A leg on another slot
        // states none: written per leg with the leg's own slot, the
        // combination carried a number this tag takes on a single order only.
        assert!(
            !f.iter().any(|field| field.starts_with("6086=")),
            "the combination states no slot of its own: {msg}",
        );
    }

    /// And where a leg is shorted out of the borrowable pool, the combination
    /// says so once, as the one value that tag carries on a combination.
    #[test]
    fn a_combination_states_it_is_a_short_sale_once() {
        use std::io::Read;
        let (mut conn, mut peer, mut context, instrument) = combo_test_state();
        let leg = |short_sale_slot: u8| crate::types::ComboLegSpec {
            con_id: 265598,
            ratio: 1,
            is_sell: true,
            exchange: String::new(),
            open_close: 0,
            short_sale_slot,
            designated_location: String::new(),
            exempt_code: -1,
            price: None,
        };
        let attrs = crate::types::OrderAttrs {
            combo_legs: vec![leg(1), leg(1)],
            ..Default::default()
        };
        send_order_ex(
            &mut conn, &mut context, &shared_for_test(), "DU123456", 33, instrument, Side::Sell,
            1, crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
            b'0', &attrs,
        )
        .unwrap();

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]);
        let f: Vec<&str> = msg.split('\u{1}').collect();
        assert_eq!(
            f.iter().filter(|field| **field == "6086=1").count(), 1,
            "once for the combination, whatever its legs: {msg}",
        );
        // And a row per leg, keyed, each with the three fields the group
        // carries — the leg that states no short sale states them empty, as a
        // fixed-width group is written.
        assert_eq!(
            f.iter().filter(|field| field.starts_with("654=")).count(), 2,
            "a key per leg: {msg}",
        );
        assert_eq!(
            f.iter().filter(|field| field.starts_with("6215=")).count(), 2,
            "and the flag per leg: {msg}",
        );
    }

    /// A ladder can start against a position already held and a first
    /// component already partly filled. Left out, the venue starts from
    /// nothing and works components the caller has already had.
    #[test]
    fn a_ladder_states_what_it_starts_from() {
        use std::io::Read;
        let (mut conn, mut peer, mut context, instrument) = combo_test_state();
        let attrs = crate::types::OrderAttrs {
            scale: Some(Box::new(crate::types::ScaleAttrs {
                init_level_size: 100,
                subs_level_size: 50,
                price_increment: crate::types::PRICE_SCALE / 100,
                init_position: 250,
                init_fill_qty: 40,
                ..Default::default()
            })),
            ..Default::default()
        };
        send_order_ex(
            &mut conn, &mut context, &shared_for_test(), "DU123456", 41, instrument, Side::Buy, 1,
            crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
            b'0', &attrs,
        )
        .unwrap();
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]);
        let f: Vec<&str> = msg.split('\u{1}').collect();
        assert!(f.contains(&"6485=250"), "the position it starts against: {msg}");
        // Not sent: the venue answers "Can not contain field # 6486".
        assert!(!msg.contains("6486="), "a field the venue will not take: {msg}");
    }

    /// Where an order's commission goes. Taken from a caller and dropped, the
    /// commission went wherever the account's default sends it.
    #[test]
    fn an_order_states_its_soft_dollar_tier() {
        use std::io::Read;
        let (mut conn, mut peer, mut context, instrument) = combo_test_state();
        let attrs = crate::types::OrderAttrs {
            soft_dollar_tier_name: "Tier A".to_string(),
            soft_dollar_tier_val: "45.5".to_string(),
            ..Default::default()
        };
        send_order_ex(
            &mut conn, &mut context, &shared_for_test(), "DU123456", 42, instrument, Side::Buy, 1,
            crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
            b'0', &attrs,
        )
        .unwrap();
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]);
        let f: Vec<&str> = msg.split('\u{1}').collect();
        assert!(f.contains(&"6519=Tier A"), "the tier: {msg}");
        assert!(f.contains(&"6520=45.5"), "what it is worth: {msg}");
        // Not sent: the venue answers "Invalid value in field # 8016".
        assert!(!msg.contains("8016="), "a field the venue will not take: {msg}");
    }

    /// A tier named with nothing against it is not an arrangement, and half of
    /// one is worse than none.
    #[test]
    fn half_a_soft_dollar_arrangement_states_nothing() {
        use std::io::Read;
        let (mut conn, mut peer, mut context, instrument) = combo_test_state();
        let attrs = crate::types::OrderAttrs {
            soft_dollar_tier_name: "Tier A".to_string(),
            ..Default::default()
        };
        send_order_ex(
            &mut conn, &mut context, &shared_for_test(), "DU123456", 43, instrument, Side::Buy, 1,
            crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
            b'0', &attrs,
        )
        .unwrap();
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        assert!(!String::from_utf8_lossy(&buf[..n]).contains("6519="), "half an arrangement");
    }

    /// Who settles an order, and whether discretion runs to the limit price.
    /// Both were taken from a caller and dropped.
    #[test]
    fn an_order_states_who_settles_it_and_how_far_discretion_runs() {
        use std::io::Read;
        let (mut conn, mut peer, mut context, instrument) = combo_test_state();
        let attrs = crate::types::OrderAttrs {
            settling_firm: "FIRM".to_string(),
            discretionary_up_to_limit: true,
            ..Default::default()
        };
        send_order_ex(
            &mut conn, &mut context, &shared_for_test(), "DU123456", 44, instrument, Side::Buy, 1,
            crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
            b'0', &attrs,
        )
        .unwrap();
        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]);
        let f: Vec<&str> = msg.split('\u{1}').collect();
        assert!(f.contains(&"6282=FIRM"), "who settles it: {msg}");
        assert!(f.contains(&"8165=1"), "how far discretion runs: {msg}");
    }

    /// A caller can price the legs separately rather than pricing the
    /// combination. Dropped, the combination is worked at whatever the venue
    /// makes of it, which is not the order that was placed.
    #[test]
    fn legs_priced_separately_go_out_with_their_prices() {
        use std::io::Read;
        let (mut conn, mut peer, mut context, instrument) = combo_test_state();
        let leg = |con_id: i64, is_sell: bool, price: Option<crate::types::Price>| {
            crate::types::ComboLegSpec {
                con_id, ratio: 1, is_sell, exchange: String::new(),
                open_close: 0, short_sale_slot: 0, designated_location: String::new(),
                exempt_code: -1, price,
            }
        };
        let attrs = crate::types::OrderAttrs {
            combo_legs: vec![
                leg(265598, false, Some(2 * crate::types::PRICE_SCALE)),
                leg(272093, true, None),
            ],
            ..Default::default()
        };
        send_order_ex(
            &mut conn, &mut context, &shared_for_test(), "DU123456", 32, instrument, Side::Buy, 1,
            crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
            b'0', &attrs,
        )
        .unwrap();

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]);
        let f: Vec<&str> = msg.split('\u{1}').collect();
        let priced: Vec<&&str> = f.iter().filter(|x| x.starts_with("6879=")).collect();
        // One price for the leg the caller priced, and nothing at all for the
        // one it left alone: an empty price tag is a statement about that
        // leg's price, which the caller did not make.
        assert_eq!(priced.len(), 1, "a price only where the caller gave one: {msg}");
        assert_eq!(*priced[0], "6879=2", "the leg the caller priced: {msg}");
    }

    /// Nothing goes out where the caller priced the combination itself, which
    /// is what most callers do.
    #[test]
    fn an_unpriced_combination_states_no_leg_prices() {
        use std::io::Read;
        let (mut conn, mut peer, mut context, instrument) = combo_test_state();
        let attrs = crate::types::OrderAttrs {
            combo_legs: vec![crate::types::ComboLegSpec {
                con_id: 265598, ratio: 1, is_sell: false, exchange: String::new(),
                open_close: 0, short_sale_slot: 0, designated_location: String::new(),
                exempt_code: -1, price: None,
            }],
            ..Default::default()
        };
        send_order_ex(
            &mut conn, &mut context, &shared_for_test(), "DU123456", 33, instrument, Side::Buy, 1,
            crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
            b'0', &attrs,
        )
        .unwrap();

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        assert!(
            !String::from_utf8_lossy(&buf[..n]).contains("6879="),
            "a price nobody stated",
        );
    }

    /// A ladder and a hedge each go out under the tags the vendor's own
    /// attributes declare for them. Without them an order asking for either
    /// would go out plain: one order for the whole size, or a position with
    /// nothing against it.
    #[test]
    fn a_scale_and_a_hedge_go_out_under_their_own_tags() {
        use std::io::Read;
        let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.set_symbol(instrument, "SPY".to_string());
        let attrs = crate::types::OrderAttrs {
            scale: Some(Box::new(crate::types::ScaleAttrs {
                init_level_size: 100,
                subs_level_size: 50,
                price_increment: crate::types::PRICE_SCALE / 20,
                profit_offset: crate::types::PRICE_SCALE / 10,
                price_adjust_interval: 60,
                auto_reset: true,
                random_percent: true,
                ..Default::default()
            })),
            delta_neutral: Some(Box::new(crate::types::DeltaNeutralAttrs {
                order_type: "MKT".into(),
                aux_price: 0,
                con_id: 265598,
            })),
            ..Default::default()
        };
        send_order_ex(
            &mut conn,
            &mut context,
            &shared_for_test(),
            "DU123456",
            21,
            instrument,
            Side::Buy,
            100,
            crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
            b'0',
            &attrs,
        )
        .unwrap();

        let mut buf = [0u8; 4096];
        let n = peer.read(&mut buf).unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]);
        let has = |t: &str| msg.split('\u{1}').any(|f| f.starts_with(t));
        for tag in [
            "6403=100",
            "6445=50",
            "6405=0.05",
            "6446=0.1",
            "6526=60",
            "6461=1",
            "6795=1",
            "6290=MKT",
            "6150=265598",
        ] {
            assert!(has(tag), "{tag} is on the order: {msg}");
        }
    }

    /// A contract that is not a stock is named by more than its symbol, and an
    /// order that states only the symbol names a whole family — which the venue
    /// answers as ambiguous, or as a contract it does not know. One submit path
    /// restated the identity and the rest did not, so which of them an order
    /// went through decided whether it could be placed at all.
    #[test]
    fn every_submit_path_names_the_contract_and_not_just_its_symbol() {
        let cases: Vec<SubmitCase> = vec![
            ("limit gtc", |c, i, o| {
                c.submit(i, Side::Buy, 1, crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE }, b'1', crate::types::OrderAttrs { outside_rth: o, ..Default::default() })
            }),
            ("stop gtc", |c, i, o| {
                c.submit(i, Side::Sell, 1, crate::types::OrderKind::Stop { stop_price: 90 * crate::types::PRICE_SCALE }, b'1', crate::types::OrderAttrs { outside_rth: o, ..Default::default() })
            }),
            ("stop limit gtc", |c, i, o| {
                c.submit(i, Side::Sell, 1, crate::types::OrderKind::StopLimit { price: 89 * crate::types::PRICE_SCALE, stop_price: 90 * crate::types::PRICE_SCALE }, b'1', crate::types::OrderAttrs { outside_rth: o, ..Default::default() })
            }),
            ("limit ioc", |c, i, _| {
                c.submit(i, Side::Buy, 1, crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE }, b'3', crate::types::OrderAttrs { outside_rth: false, ..Default::default() })
            }),
            ("limit fok", |c, i, _| {
                c.submit(i, Side::Buy, 1, crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE }, b'4', crate::types::OrderAttrs { outside_rth: false, ..Default::default() })
            }),
        ];

        for (label, submit) in cases {
            let mut context = Context::new();
            let instrument = context
                .market
                .try_register_contract(893091670, "MES", "FUT", "CME", "20270917|0||5|MES|MESU7")
                .expect("register a future");
            context.set_symbol(instrument, "MES".to_string());
            submit(&mut context, instrument, false);
            let sent = drain(&mut context);

            // A future states its maturity on the tag carrying the form it was
            // given: a contract month on tag 200, a full date on tag 541. A
            // future does not always stop trading in the month it is named for,
            // so a truncated date names a different contract.
            assert!(sent.contains("|541=20270917|"), "{label} states the maturity date: {sent}");
            assert!(!sent.contains("|200="), "{label} states no contract month: {sent}");
            assert!(sent.contains("|231=5|"), "{label} states the multiplier: {sent}");

            // The same contract named by its month rides the month tag.
            let mut by_month = Context::new();
            let monthly = by_month
                .market
                .try_register_contract(893091670, "MES", "FUT", "CME", "202709|0||5|MES|MESU7")
                .expect("register a future by its month");
            by_month.set_symbol(monthly, "MES".to_string());
            submit(&mut by_month, monthly, false);
            let monthly_sent = drain(&mut by_month);
            assert!(
                monthly_sent.contains("|200=202709|"),
                "{label} states a month on the month tag: {monthly_sent}",
            );
            assert!(!monthly_sent.contains("|541="), "{label} states no date: {monthly_sent}");
            assert!(sent.contains("|167=FUT|"), "{label} states the security type: {sent}");
            // The member, not the family: the local symbol on tag 48 under
            // source `101`.
            assert!(!sent.contains("|6058="), "{label} states no trading class: {sent}");
            assert!(sent.contains("|48=MESU7|"), "{label} names the contract: {sent}");
            assert!(sent.contains("|22=101|"), "{label} says what the identifier is: {sent}");
            // An order that asked for neither states neither. A zero percent
            // offset is a relative order and a zero exempt code is a short
            // sale exemption, so a derived default put both on every order.
            assert!(!sent.contains("|9822="), "{label} claims no percent offset: {sent}");
            assert!(!sent.contains("|1688="), "{label} claims no exemption: {sent}");
            assert!(!sent.contains("|21="), "{label} states no handling instruction: {sent}");
            assert!(sent.contains("|204=0|"), "{label} says who the order is for: {sent}");
        }
    }

    /// The paths that build their own frame rather than going through the
    /// shared encoder. Each states the contract identity tags, so a bracket on
    /// a future names the contract and not the family.
    #[test]
    fn a_bracket_and_a_fraction_name_the_contract_on_every_leg() {
        let mut context = Context::new();
        let instrument = context
            .market
            .try_register_contract(893091670, "MES", "FUT", "CME", "20270917|0||5|MES|MESU7")
            .expect("register a future");
        context.set_symbol(instrument, "MES".to_string());
        context.pending_orders.push(crate::types::OrderRequest::SubmitBracket { con_id: 0,
            parent_id: 1,
            tp_id: 2,
            sl_id: 3,
            instrument,
            side: Side::Buy,
            qty: crate::types::QTY_SCALE,
            entry_price: 100 * crate::types::PRICE_SCALE,
            take_profit: 110 * crate::types::PRICE_SCALE,
            stop_loss: 90 * crate::types::PRICE_SCALE,
        });
        let sent = drain(&mut context);
        assert_eq!(
            sent.matches("|48=MESU7|").count(),
            3,
            "all three bracket legs name the contract: {sent}",
        );
        assert_eq!(
            sent.matches("|6008=893091670|").count(),
            3,
            "all three bracket legs carry the contract id: {sent}",
        );

        context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
            order_id: 4,
            instrument,
            side: Side::Buy,
            qty: crate::types::QTY_SCALE / 2,
            kind: crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
            tif: b'0',
            attrs: Default::default(),
        });
        let sent = drain(&mut context);
        assert!(sent.contains("|48=MESU7|"), "the fraction names the contract: {sent}");
        assert!(sent.contains("|6008=893091670|"), "the fraction carries the id: {sent}");
    }

    /// A cancel states tag 38 as the quantity the order carries, so a
    /// fractional order is cancelled for the fraction it was placed for
    /// rather than for a quantity of zero.
    #[test]
    fn a_fractional_cancel_states_the_fraction_it_was_placed_for() {
        let mut context = Context::new();
        let instrument = context.register_instrument(893091670);
        context.set_symbol(instrument, "MES".to_string());
        context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
            order_id: 9,
            instrument,
            side: Side::Buy,
            qty: crate::types::QTY_SCALE / 2,
            kind: crate::types::OrderKind::Limit { price: 100 * crate::types::PRICE_SCALE },
            tif: b'0',
            attrs: Default::default(),
        });
        drain(&mut context);
        context.pending_orders.push(crate::types::OrderRequest::Cancel { order_id: 9 });
        let sent = drain(&mut context);
        assert!(!sent.contains("|38=0|"), "the cancel states no zero quantity: {sent}");
        assert!(sent.contains("|38=0.5|"), "it states the fraction instead: {sent}");
    }

    /// A replace restates the order's terms, not its history: `filled` carries
    /// forward across the replacement.
    #[test]
    fn a_replace_keeps_what_the_order_has_already_filled() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.set_symbol(instrument, "SPY".to_string());
        context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
            order_id: 11,
            instrument,
            side: Side::Buy,
            qty: 100 * crate::types::QTY_SCALE,
            kind: crate::types::OrderKind::Limit { price: 150 * crate::types::PRICE_SCALE },
            tif: b'0',
            attrs: Default::default(),
        });
        drain(&mut context);
        context.update_order_status(11, OrderStatus::Submitted, false);
        context.adjust_order_filled(11, 40 * crate::types::QTY_SCALE);
        context.pending_orders.push(crate::types::OrderRequest::Modify {
            order_id: 11,
            price: 151 * crate::types::PRICE_SCALE,
            qty: 100 * crate::types::QTY_SCALE,
            outside_rth: false,
            ord_type: 0,
            tif: 0,
            stop_price: 0,
            spec: None,
        });
        drain(&mut context);
        assert_eq!(
            context.order(11).map(|o| o.filled),
            Some(40 * crate::types::QTY_SCALE),
            "the replace kept the 40 already filled",
        );
    }

    /// An order the venue is holding can go back to working, so a request to
    /// withdraw everything on a contract has to reach it.
    #[test]
    fn cancelling_everything_reaches_an_order_the_venue_is_holding() {
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.set_symbol(instrument, "SPY".to_string());
        context.insert_order(crate::types::Order::new(
            21, instrument, Side::Buy, crate::types::QTY_SCALE, 100 * crate::types::PRICE_SCALE, b'2', b'0', 0,
        ));
        context.update_order_status(21, OrderStatus::Inactive, false);
        context.pending_orders.push(crate::types::OrderRequest::CancelAll { instrument });
        let sent = drain(&mut context);
        assert!(sent.contains("|11=C21|"), "the held order was cancelled: {sent}");
    }
}

/// A delayed activation goes out on tag 168, dash-joined and in UTC. That is
/// not the form this client's other timestamps take; the space-joined form is
/// read as a different moment.
#[test]
fn a_delayed_activation_is_written_the_way_it_is_read() {
    let order = crate::types::model::Order {
        action: "BUY".into(), total_quantity: 1.0, order_type: "LMT".into(),
        lmt_price: 100.0, tif: "DAY".into(),
        good_after_time: "20260311 09:30:00".into(),
        ..Default::default()
    };
    let attrs = order.attrs();
    assert_eq!(attrs.good_after, 1_773_221_400, "read as UTC");

    let mut fields: Vec<(u32, String)> = Vec::new();
    push_order_attrs(
        &mut fields, &attrs, &crate::types::OrderKind::Limit { price: 100_0000_0000 },
        Side::Buy, String::new(),
    );
    let stated = fields.iter().find(|(t, _)| *t == 168).map(|(_, v)| v.as_str());
    assert_eq!(stated, Some("20260311-09:30:00"), "sent on 168: {fields:?}");
}

/// What this client writes about an order, it reads back.
///
/// The two sides are written in different files and drifted: the smallest
/// fill quantity was sent on one tag and read from none, the time a manual
/// order was entered was sent on one tag and read from another, and three
/// flags were sent as `Y` or `1` and read by a parser that accepted neither.
/// Each of those came back as though the caller had asked for nothing.
///
/// So the check is the round trip rather than either side of it: state the
/// attributes, write them, read them back, and compare.
#[test]
fn what_this_client_writes_about_an_order_it_reads_back() {
    let attrs = crate::types::OrderAttrs {
        min_qty: 25,
        manual_order_time: "20260311-09:30:00".to_string(),
        manual_order_indicator: 1,
        route_marketable_to_bbo: true,
        block_order: true,
        display_size: 100,
        not_held: true,
        sweep_to_fill: true,
        solicited: true,
        post_only: true,
        discretionary_amt: crate::types::price_from_f64(0.05),
        trigger_method: 2,
        clearing_account: "CA-1".to_string(),
        ..Default::default()
    };
    let mut fields: Vec<(u32, String)> = Vec::new();
    super::push_order_attrs(
        &mut fields, &attrs, &crate::types::OrderKind::Limit { price: 100_0000_0000 },
        Side::Buy, String::new(),
    );
    let on_the_wire: std::collections::HashMap<u32, String> = fields.into_iter().collect();

    let mut read = crate::types::model::Order::default();
    crate::engine::hot_loop::ccp::executions::read_stated_attributes(&mut read, &on_the_wire);

    assert_eq!(read.min_qty, 25, "the smallest fill it may take: {on_the_wire:?}");
    assert_eq!(read.manual_order_time, "20260311-09:30:00", "when it was entered by hand");
    assert_eq!(read.manual_order_indicator, 1, "that it was entered by hand");
    assert_eq!(read.route_marketable_to_bbo, Some(true), "that it routes to the best offer");
    assert!(read.block_order, "that it is a block order");
    assert_eq!(read.display_size, 100, "how much of it shows");
    assert!(read.not_held, "that it is not held");
    assert!(read.sweep_to_fill, "that it sweeps to fill");
    assert!(read.solicited, "that it was solicited");
    assert!(read.post_only, "that it only posts");
    assert_eq!(read.discretionary_amt, 0.05, "what it may move by");
    assert_eq!(read.trigger_method, 2, "what triggers it");
    assert_eq!(read.clearing_account, "CA-1", "where it clears");
}

/// A block order states tag 9801 as the character `Y`.
///
/// A numeric `1` is not read on this tag, and the tag is omitted when the flag
/// is off.
#[test]
fn a_block_order_states_the_character_the_protocol_defines() {
    let stated = |block: bool| {
        let attrs = crate::types::OrderAttrs { block_order: block, ..Default::default() };
        let mut fields: Vec<(u32, String)> = Vec::new();
        super::push_order_attrs(
            &mut fields,
            &attrs,
            &crate::types::OrderKind::Market,
            Side::Buy,
            String::new(),
        );
        fields
    };
    let on = stated(true);
    assert!(
        on.iter().any(|(t, v)| *t == 9801 && v == "Y"),
        "a block order states Y, got {:?}",
        on.iter().filter(|(t, _)| *t == 9801).collect::<Vec<_>>(),
    );

    let off = stated(false);
    assert!(
        !off.iter().any(|(t, _)| *t == 9801),
        "an order that is not a block order states nothing",
    );
}

/// A manual order states tag 1028 as the character `Y` or `N`.
///
/// Tag 1028 carries a character, not a number: any other value reads as
/// unstated. The tag is omitted when the caller states nothing.
#[test]
fn a_manual_order_states_the_character_the_protocol_defines() {
    let stated = |indicator: i32| {
        let mut fields: Vec<(u32, String)> = Vec::new();
        let attrs = crate::types::OrderAttrs {
            manual_order_indicator: indicator,
            ..crate::types::OrderAttrs::default()
        };
        push_order_attrs(
            &mut fields,
            &attrs,
            &crate::types::OrderKind::Market,
            Side::Buy,
            String::new(),
        );
        fields.iter().find(|(t, _)| *t == 1028).map(|(_, v)| v.clone())
    };
    assert_eq!(stated(1).as_deref(), Some("Y"), "entered by hand");
    assert_eq!(stated(0).as_deref(), Some("N"), "entered by a program");
    assert_eq!(stated(i32::MAX), None, "the caller stated nothing");
}

/// An adjustable stop names the type it becomes by that type's own code.
///
/// Tag 6261 carries the order-type code: `3` for a stop, `4` for a stop limit,
/// `T` for a trailing stop and `TSL` for a trailing stop limit. `7` and `8` are
/// not order-type codes.
#[test]
fn an_adjustable_conversion_names_the_type_the_registry_names() {
    use crate::types::AdjustedOrderType as A;
    let stated = |adjusted: A| {
        let msg = send_kind_for_test(
            crate::types::OrderKind::AdjustableStop {
                stop_price: 11 * crate::types::PRICE_SCALE,
                trigger_price: 12 * crate::types::PRICE_SCALE,
                adjusted_order_type: adjusted,
                adjusted_stop_price: 11 * crate::types::PRICE_SCALE,
                adjusted_stop_limit_price: 0,
                adjusted_trailing_amount: crate::types::PRICE_SCALE,
                adjustable_trailing_unit: 0,
            },
            b'1',
            crate::types::OrderAttrs::default(),
        );
        msg.split('\u{1}').find_map(|f| f.strip_prefix("6261=").map(str::to_string))
    };
    assert_eq!(stated(A::Stop).as_deref(), Some("3"));
    assert_eq!(stated(A::StopLimit).as_deref(), Some("4"));
    assert_eq!(stated(A::Trail).as_deref(), Some("T"));
    assert_eq!(stated(A::TrailLimit).as_deref(), Some("TSL"));
}

/// A stop limit is previewed with both of its prices.
///
/// The limit rides tag 44 and the trigger tag 99. A preview stating only one of
/// them describes an order the venue will not accept.
#[test]
fn a_stop_limit_preview_states_both_of_its_prices() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::StopLimit {
            price: 95 * crate::types::PRICE_SCALE,
            stop_price: 90 * crate::types::PRICE_SCALE,
        },
        b'0',
        crate::types::OrderAttrs { what_if: true, ..Default::default() },
    );
    assert!(msg.contains("\u{1}44=95\u{1}"), "the limit is stated: {msg}");
    assert!(msg.contains("\u{1}99=90\u{1}"), "the trigger is stated: {msg}");
}

/// A pegged-to-benchmark order states no price on tag 44.
///
/// The wire shape of the type carries the peg's own fields and its starting
/// price on tag 99, and nothing on tag 44 — a limit the caller named for the
/// peg is not among the fields the venue takes for it, so the submit must
/// not grow one.
#[test]
fn a_benchmark_peg_states_no_price_on_tag_44() {
    let msg = send_kind_for_test(
        crate::types::OrderKind::PegBench {
            price: 150 * crate::types::PRICE_SCALE,
            ref_con_id: 756733,
            is_peg_decrease: false,
            pegged_change_amount: crate::types::PRICE_SCALE,
            ref_change_amount: crate::types::PRICE_SCALE,
            starting_price: 149 * crate::types::PRICE_SCALE,
            stock_ref_price: 149 * crate::types::PRICE_SCALE,
            ref_exchange: "SMART".into(),
        },
        b'0',
        crate::types::OrderAttrs::default(),
    );
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("44="), None, "no price is stated on tag 44: {msg}");
    assert_eq!(tag("40=").as_deref(), Some("PB"), "and it is still a benchmark peg: {msg}");
    assert_eq!(tag("99=").as_deref(), Some("149"), "the starting price keeps its own tag: {msg}");
}

/// A cancel-all sends one frame per order and reports one outcome per order.
/// `CancelAll` carries no order id of its own, so a single result for the set
/// cannot name the order whose cancel failed.
#[test]
fn a_cancel_all_names_every_order_whose_cancel_did_not_leave() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    // The send fails on the call rather than after a buffer fills.
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    for id in [41u64, 42, 43] {
        context.insert_order(crate::types::Order::new(
            id, instrument, Side::Buy, 100 * crate::types::QTY_SCALE, 150 * crate::types::PRICE_SCALE, b'2', b'0', 0,
        ));
        context.set_order_status_forced(id, OrderStatus::Submitted);
    }
    context.cancel_all(instrument);

    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);

    for id in [41u64, 42, 43] {
        assert_eq!(
            context.order(id).map(|o| o.status),
            Some(OrderStatus::Uncertain),
            "order {id} was not cancelled and is not known to be working",
        );
    }
    let told = shared.orders.drain_order_updates();
    for id in [41u64, 42, 43] {
        let update = told.iter().find(|u| u.order_id == id).expect("every order is reported");
        assert_eq!(update.status, OrderStatus::Uncertain);
        // Instrument 0 is a valid instrument id, so a zeroed update names
        // another contract's order.
        assert_eq!(update.instrument, instrument, "the contract it is on");
        assert_eq!(update.remaining_qty, 100.0, "and what is still outstanding on it");
    }
}

/// A price is sent as stated, on the contract's tick grid or not. The venue
/// rejects an off-grid price rather than adjusting it.
#[test]
fn an_off_grid_price_reaches_the_venue_as_the_caller_stated_it() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.market.set_min_tick(instrument, 0.05);

    // 149.03 is off a five-cent grid.
    let off_grid = 149 * crate::types::PRICE_SCALE + 3 * crate::types::PRICE_SCALE / 100;
    context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
        order_id: 7,
        instrument,
        side: Side::Buy,
        qty: crate::types::QTY_SCALE,
        kind: crate::types::OrderKind::Limit { price: off_grid },
        tif: b'0',
        attrs: crate::types::OrderAttrs::default(),
    });
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);

    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
    assert!(msg.contains("|44=149.03|"), "the price the caller stated: {msg}");
}

/// A replace states the whole order, so an untracked order has nothing to
/// restate. The request is refused rather than sent under defaults that name no
/// order the venue holds.
#[test]
fn a_replace_for_an_untracked_order_is_refused_rather_than_invented() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 4242,
        price: 100 * crate::types::PRICE_SCALE,
        qty: crate::types::QTY_SCALE,
        outside_rth: false,
        ord_type: 0,
        tif: 0,
        stop_price: 0,
        spec: None,
    });
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);

    let mut buf = [0u8; 4096];
    assert!(
        matches!(peer.read(&mut buf), Err(_) | Ok(0)),
        "nothing reaches the wire: {}",
        String::from_utf8_lossy(&buf),
    );
    let told = shared.orders.drain_order_inactive();
    assert!(
        told.iter().any(|(id, _, _)| *id == 4242),
        "and the caller is told which order could not be replaced: {told:?}",
    );
}

/// What a replace carrying a new trail actually puts on the wire.
///
/// The venue accepts such a replace — measured on a paper session — so the
/// question is only which trail it is told. This client restates the trail
/// from the record the order was placed under, and a caller naming a new one
/// has to see that number reach the venue, or the two disagree with nothing
/// saying so.
#[test]
fn a_replace_naming_a_new_trail_puts_that_trail_on_the_wire() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());

    context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
        order_id: 42,
        instrument,
        side: Side::Sell,
        qty: crate::types::QTY_SCALE,
        kind: crate::types::OrderKind::TrailingStop {
            trail_stop_price: 0,
            trail_amt: 5 * crate::types::PRICE_SCALE,
        },
        tif: b'0',
        attrs: crate::types::OrderAttrs::default(),
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let mut buf = [0u8; 8192];
    let _placed = peer.read(&mut buf).expect("the order reaches the peer");

    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 42,
        price: 0,
        qty: crate::types::QTY_SCALE,
        outside_rth: false,
        ord_type: 0,
        tif: 0,
        // The trail is the auxiliary price, which is what this field carries.
        stop_price: 9 * crate::types::PRICE_SCALE,
        spec: None,
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));

    assert_eq!(tag("35=").as_deref(), Some("G"), "a replace was sent: {msg}");
    assert_eq!(
        tag("211=").as_deref(), Some("9"),
        "the replace carries the trail the caller named, not the one the order was \
         placed with: {msg}",
    );
}

/// A placed order, replaced naming a price and a trigger, and the frame the
/// replace put on the wire.
fn replace_frame(kind: crate::types::OrderKind, price: i64, stop_price: i64) -> String {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    context.pending_orders.push(crate::types::OrderRequest::SubmitEx {
        con_id: 0, order_id: 42, instrument, side: Side::Buy, qty: crate::types::QTY_SCALE,
        kind, tif: b'0', attrs: crate::types::OrderAttrs::default(),
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let mut buf = [0u8; 8192];
    let _placed = peer.read(&mut buf).expect("the order reaches the peer");
    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 42, price, qty: crate::types::QTY_SCALE, outside_rth: false,
        ord_type: 0, tif: 0, stop_price,
        spec: None,
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let n = peer.read(&mut buf).unwrap();
    String::from_utf8_lossy(&buf[..n]).to_string()
}

/// A placed order replaced twice, and the frame the second replace put on the
/// wire.
fn second_replace_frame(kind: crate::types::OrderKind, first: (i64, i64), second: (i64, i64)) -> String {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    context.pending_orders.push(crate::types::OrderRequest::SubmitEx {
        con_id: 0, order_id: 42, instrument, side: Side::Buy, qty: crate::types::QTY_SCALE,
        kind, tif: b'0', attrs: crate::types::OrderAttrs::default(),
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let mut buf = [0u8; 8192];
    let _placed = peer.read(&mut buf).expect("the order reaches the peer");
    for (price, stop_price) in [first, second] {
        context.pending_orders.push(crate::types::OrderRequest::Modify {
            order_id: 42, price, qty: crate::types::QTY_SCALE, outside_rth: false,
            ord_type: 0, tif: 0, stop_price,
            spec: None,
        });
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
        let n = peer.read(&mut buf).unwrap();
        if (price, stop_price) == second {
            return String::from_utf8_lossy(&buf[..n]).to_string();
        }
    }
    unreachable!()
}

/// Every statement of one tag on a frame, in order.
fn stated(msg: &str, tag: &str) -> Vec<String> {
    msg.split('\u{1}').filter_map(|f| f.strip_prefix(tag).map(str::to_string)).collect()
}

/// What a replace naming a new offset or cap puts on the wire, for every
/// shape that carries one in the shape it was placed with.
///
/// Measured on a paper session, each shape placed and then replaced naming a
/// new number: the venue went on holding the placed offset of a relative
/// order, the placed cap of a midpoint peg and of a midprice order, and the
/// placed trail and limit offset of a trailing stop limit, because the replace
/// restated each from the record of the placement while the caller's number
/// went nowhere. The venue reads each number from the tag the shape's own
/// submit states it on, so that is where the replace states the new one — on
/// every tag the submit uses, and on none the caller did not name.
#[test]
fn a_replace_naming_a_new_offset_or_cap_puts_it_where_the_submit_does() {
    use crate::types::{OrderKind as K, PRICE_SCALE as P};
    let one = |tag: &str, msg: &str| {
        let all = stated(msg, tag);
        assert!(all.len() <= 1, "{tag} stated twice on one frame: {msg}");
        all.into_iter().next()
    };

    let msg = replace_frame(K::Rel { offset: 5 * P / 100 }, 0, 10 * P / 100);
    assert_eq!(one("211=", &msg).as_deref(), Some("0.1"), "a relative order's offset: {msg}");
    assert_eq!(one("99=", &msg).as_deref(), Some("0.1"), "and the trigger tag agrees with it: {msg}");

    let msg = replace_frame(K::PegMid { offset: 0, price_cap: 100 * P }, 101 * P, 0);
    assert_eq!(one("44=", &msg).as_deref(), Some("101"), "a midpoint peg's cap: {msg}");
    assert_eq!(one("211=", &msg).as_deref(), Some("0"), "and the offset it was placed with stays: {msg}");

    let msg = replace_frame(K::MidPrice { price_cap: 100 * P }, 101 * P, 0);
    assert_eq!(one("44=", &msg).as_deref(), Some("101"), "a midprice order's cap: {msg}");

    let msg = replace_frame(K::SnapMid { offset: 5 * P / 100 }, 0, 10 * P / 100);
    assert_eq!(one("211=", &msg).as_deref(), Some("0.1"), "a snap's offset: {msg}");
    assert_eq!(one("99=", &msg).as_deref(), Some("0.1"), "on both tags the venue states it on: {msg}");

    let msg = replace_frame(
        K::TrailingStopLimit { lmt_offset: 10 * P / 100, trail_amt: P, trail_stop_price: 0 },
        20 * P / 100, 2 * P,
    );
    assert_eq!(one("99=", &msg).as_deref(), Some("2"), "a trailing stop limit's trail: {msg}");
    assert_eq!(one("211=", &msg).as_deref(), Some("2"), "on both of its tags: {msg}");
    assert_eq!(one("6370=", &msg).as_deref(), Some("0.2"), "and its limit offset: {msg}");

    // A replace naming nothing leaves every placed number in force, which is
    // how a caller moves the quantity alone.
    let msg = replace_frame(K::TrailingStopLimit { lmt_offset: 10 * P / 100, trail_amt: P, trail_stop_price: 0 }, 0, 0);
    assert_eq!((one("211=", &msg).as_deref(), one("6370=", &msg).as_deref()), (Some("1"), Some("0.1")), "{msg}");
}

/// What a second replace restates is what the first one moved to, not what
/// the order was placed with.
///
/// The restatement reads the shape off the record of the placement. A caller
/// who moved an offset and then moved the quantity alone had the placed
/// offset written back onto the peg tag by the second replace, beside a
/// trigger tag carrying the moved one — and the venue reads the peg tag.
#[test]
fn a_second_replace_restates_what_the_first_one_moved_to() {
    use crate::types::{OrderKind as K, PRICE_SCALE as P};
    let one = |tag: &str, msg: &str| stated(msg, tag).into_iter().next();

    let msg = second_replace_frame(K::Rel { offset: 5 * P / 100 }, (0, 10 * P / 100), (0, 0));
    assert_eq!(one("211=", &msg).as_deref(), Some("0.1"), "a relative order's moved offset stands: {msg}");
    assert_eq!(one("99=", &msg).as_deref(), Some("0.1"), "{msg}");

    let msg = second_replace_frame(K::PegMid { offset: 0, price_cap: 100 * P }, (101 * P, 0), (0, 0));
    assert_eq!(one("44=", &msg).as_deref(), Some("101"), "a midpoint peg's moved cap stands: {msg}");

    let msg = second_replace_frame(
        K::TrailingStopLimit { lmt_offset: 10 * P / 100, trail_amt: P, trail_stop_price: 0 },
        (20 * P / 100, 2 * P), (0, 0),
    );
    assert_eq!(
        (one("211=", &msg).as_deref(), one("6370=", &msg).as_deref()), (Some("2"), Some("0.2")),
        "a trailing stop limit's moved trail and offset stand: {msg}",
    );

    let msg = second_replace_frame(K::TrailingStop { trail_stop_price: 0, trail_amt: 5 * P }, (0, 9 * P), (0, 0));
    assert_eq!(one("211=", &msg).as_deref(), Some("9"), "a trailing stop's moved trail stands: {msg}");
}

/// A replace of an order this session did not place restates the shape from
/// the caller's own statement of it.
///
/// An order known only from the venue's naming at connect has no record here,
/// and a pegged, snap or trailing order could not be replaced at all — refused
/// as one whose defining number cannot be restated. The reference client sends
/// whatever the caller states on a modify; the surfaces send that statement
/// ahead of the replace, and the engine keeps it as the record where it has
/// none.
#[test]
fn a_replace_of_an_order_this_session_did_not_place_restates_what_the_caller_states() {
    use std::io::Read;
    use crate::types::{OrderKind as K, PRICE_SCALE as P};
    let modify = |stated: bool| crate::types::OrderRequest::Modify {
        order_id: 77, price: 101 * P, qty: 2 * crate::types::QTY_SCALE, outside_rth: false,
        ord_type: 0, tif: 0, stop_price: 0,
        spec: stated.then(|| Box::new(crate::types::OrderSpec {
            kind: K::PegMid { offset: 0, price_cap: 100 * P },
            attrs: crate::types::OrderAttrs::default(),
        })),
    };
    for described in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_millis(300))).unwrap();
        let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        context.set_symbol(instrument, "SPY".to_string());
        // Named by the venue at connect: in the book, with no record here.
        context.insert_order(crate::types::Order::new(
            77, instrument, Side::Buy, crate::types::QTY_SCALE, 0, crate::types::ORD_PEG_MID, b'0', 0,
        ));
        let mut hb = crate::engine::hot_loop::HeartbeatState::new();
        let shared = std::sync::Arc::new(SharedState::new());
        context.pending_orders.push(modify(described));
        drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
        let mut buf = [0u8; 8192];
        let n = peer.read(&mut buf).unwrap_or(0);
        let msg = String::from_utf8_lossy(&buf[..n]).to_string();
        let refused = shared.orders.drain_order_inactive();
        if described {
            assert_eq!(stated(&msg, "35=").first().map(String::as_str), Some("G"), "sent: {msg}");
            assert_eq!(stated(&msg, "44=").first().map(String::as_str), Some("101"), "the cap the caller named: {msg}");
            assert_eq!(stated(&msg, "211=").first().map(String::as_str), Some("0"), "and the offset the caller stated: {msg}");
            assert!(refused.is_empty(), "{refused:?}");
        } else {
            assert_eq!(n, 0, "nothing reaches the wire without a statement of the order: {msg}");
            assert!(refused.iter().any(|(id, ..)| *id == 77), "and the caller is told: {refused:?}");
        }
    }
}

/// The caller's latest statement is the one restated, whether or not this
/// session placed the order.
///
/// Every statement is the caller's, so the latest stands, as it does on the
/// reference client, where each modify carries the whole order. A record made
/// at placement used to hold against a later statement, which meant a caller
/// who changed an attribute and replaced had the first value sent to the venue
/// while this client answered "what is working" with the second: the change
/// never left the process and nothing said so.
#[test]
fn the_latest_statement_stands_however_the_order_was_first_known() {
    use std::io::Read;
    use crate::types::{OrderKind as K, PRICE_SCALE as P};
    let modify = |order_id: u64, display_size: u32| crate::types::OrderRequest::Modify {
        order_id, price: 0, qty: 2 * crate::types::QTY_SCALE, outside_rth: false, ord_type: 0, tif: 0, stop_price: 0,
        spec: Some(Box::new(crate::types::OrderSpec {
            kind: K::PegMid { offset: 0, price_cap: 100 * P },
            attrs: crate::types::OrderAttrs { display_size, ..Default::default() },
        })),
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(300))).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    let mut buf = [0u8; 8192];
    let mut frame = |context: &mut Context| {
        drain_and_send_orders(&mut conn, context, "DU1", &mut hb, false, &shared, false, &None);
        let n = peer.read(&mut buf).unwrap_or(0);
        String::from_utf8_lossy(&buf[..n]).to_string()
    };

    // Named by the venue, stated twice: the second statement shows an iceberg.
    context.insert_order(crate::types::Order::new(
        77, instrument, Side::Buy, crate::types::QTY_SCALE, 0, crate::types::ORD_PEG_MID, b'0', 0,
    ));
    context.pending_orders.push(modify(77, 0));
    let msg = frame(&mut context);
    assert!(stated(&msg, "111=").is_empty(), "stated without one: {msg}");
    context.pending_orders.push(modify(77, 10));
    let msg = frame(&mut context);
    assert_eq!(stated(&msg, "111=").first().map(String::as_str), Some("10"), "the latest statement: {msg}");

    // Placed here with no iceberg, then stated by the caller with one: the
    // replace carries the iceberg, because that is what the caller now wants.
    context.pending_orders.push(crate::types::OrderRequest::SubmitEx {
        con_id: 0, order_id: 78, instrument, side: Side::Buy, qty: crate::types::QTY_SCALE,
        kind: K::PegMid { offset: 0, price_cap: 100 * P }, tif: b'0', attrs: crate::types::OrderAttrs::default(),
    });
    let placed = frame(&mut context);
    assert!(stated(&placed, "111=").is_empty(), "placed without one: {placed}");
    context.pending_orders.push(modify(78, 10));
    let msg = frame(&mut context);
    assert_eq!(stated(&msg, "35=").first().map(String::as_str), Some("G"), "{msg}");
    assert_eq!(
        stated(&msg, "111=").first().map(String::as_str), Some("10"),
        "the attribute the caller changed reaches the venue: {msg}",
    );
}

/// A refusal puts the whole statement back, attributes included.
///
/// A later statement of a venue-named order can change its attributes as well
/// as its shape. The fallback kept against a refusal carried the shape alone,
/// so a refusal of the first revision put back its shape under the second
/// statement's attributes, and the next replace restated that hybrid.
#[test]
fn a_refusal_puts_the_whole_statement_back() {
    use std::io::Read;
    use crate::types::{OrderKind as K, PRICE_SCALE as P};
    let modify = |price: i64, qty: i64, display_size: u32| crate::types::OrderRequest::Modify {
        order_id: 77, price, qty, outside_rth: false, ord_type: 0, tif: 0, stop_price: 0,
        spec: Some(Box::new(crate::types::OrderSpec {
            kind: K::PegMid { offset: 0, price_cap: 100 * P },
            attrs: crate::types::OrderAttrs { display_size, ..Default::default() },
        })),
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(300))).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.insert_order(crate::types::Order::new(
        77, instrument, Side::Buy, crate::types::QTY_SCALE, 0, crate::types::ORD_PEG_MID, b'0', 0,
    ));
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    let mut buf = [0u8; 8192];
    let mut frame = |context: &mut Context| {
        drain_and_send_orders(&mut conn, context, "DU1", &mut hb, false, &shared, false, &None);
        let n = peer.read(&mut buf).unwrap_or(0);
        String::from_utf8_lossy(&buf[..n]).to_string()
    };
    context.pending_orders.push(modify(101 * P, crate::types::QTY_SCALE, 0));
    let first = frame(&mut context);
    assert_eq!(stated(&first, "44=").first().map(String::as_str), Some("101"), "{first}");
    context.pending_orders.push(modify(0, 2 * crate::types::QTY_SCALE, 10));
    let second = frame(&mut context);
    assert_eq!(stated(&second, "111=").first().map(String::as_str), Some("10"), "{second}");

    // The venue refuses the first revision, and every later one with it.
    context.restore_pre_replace(77, 1);

    context.pending_orders.push(modify(0, 3 * crate::types::QTY_SCALE, 0));
    let third = frame(&mut context);
    assert_eq!(stated(&third, "44=").first().map(String::as_str), Some("100"), "the shape before the refused revision: {third}");
    assert!(stated(&third, "111=").is_empty(), "and its attributes, not the later statement's: {third}");
}

/// A replace the venue refused moved nothing, so the shape goes back with the
/// terms, and the next replace restates the placed number.
#[test]
fn a_refused_replace_puts_the_shape_back_with_the_terms() {
    use std::io::Read;
    use crate::types::{OrderKind as K, PRICE_SCALE as P};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    let mut buf = [0u8; 8192];
    let mut send = |context: &mut Context, req: crate::types::OrderRequest| {
        context.pending_orders.push(req);
        drain_and_send_orders(&mut conn, context, "DU1", &mut hb, false, &shared, false, &None);
        let n = peer.read(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    };
    send(&mut context, crate::types::OrderRequest::SubmitEx {
        con_id: 0, order_id: 42, instrument, side: Side::Buy, qty: crate::types::QTY_SCALE,
        kind: K::Rel { offset: 5 * P / 100 }, tif: b'0', attrs: crate::types::OrderAttrs::default(),
    });
    let moved = send(&mut context, crate::types::OrderRequest::Modify {
        order_id: 42, price: 0, qty: crate::types::QTY_SCALE, outside_rth: false, ord_type: 0, tif: 0,
        stop_price: 10 * P / 100,
        spec: None,
    });
    assert_eq!(stated(&moved, "211=").first().map(String::as_str), Some("0.1"), "{moved}");

    // The venue refuses that revision, which the trading handler answers by
    // putting the terms the venue holds back under it.
    context.restore_pre_replace(42, 1);

    let again = send(&mut context, crate::types::OrderRequest::Modify {
        order_id: 42, price: 0, qty: 2 * crate::types::QTY_SCALE, outside_rth: false, ord_type: 0, tif: 0,
        stop_price: 0,
        spec: None,
    });
    assert_eq!(
        (stated(&again, "211=").first().map(String::as_str), stated(&again, "99=").first().map(String::as_str)),
        (Some("0.05"), Some("0.05")),
        "the refused offset is not restated: {again}",
    );
}

/// An advisor's allocation rides the order and its replacement.
///
/// The group (6160), the method (6159) and the share (6164) are stated on the
/// order in the account block both encoders share, so a replace carries the
/// same allocation the placement did. Refused by name at the call and written
/// by nothing, an order asking to be spread across a group could not be placed
/// at all — while the reports coming back named all three tags.
///
/// A share the caller left empty is left off: the venue omits a blank value,
/// and a blank tag is a statement no other client makes.
#[test]
fn an_advisor_allocation_rides_the_order_and_its_replacement() {
    use std::io::Read;
    use crate::types::{OrderKind as K, PRICE_SCALE as P};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    let mut buf = [0u8; 8192];
    let mut send = |context: &mut Context, req: crate::types::OrderRequest| {
        context.pending_orders.push(req);
        drain_and_send_orders(&mut conn, context, "DU1", &mut hb, false, &shared, false, &None);
        let n = peer.read(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    };
    let allocation = |percentage: &str| crate::types::OrderAttrs {
        fa_group: "AllAccounts".to_string(),
        fa_method: "EqualQuantity".to_string(),
        fa_percentage: percentage.to_string(),
        ..Default::default()
    };
    let spread = |msg: &str| -> Vec<(u32, String)> {
        msg.split('\u{1}')
            .filter_map(|f| f.split_once('='))
            .filter(|(t, _)| matches!(*t, "6160" | "6159" | "6164"))
            .map(|(t, v)| (t.parse().unwrap(), v.to_string()))
            .collect()
    };

    let placed = send(&mut context, crate::types::OrderRequest::SubmitEx {
        con_id: 0, order_id: 31, instrument, side: Side::Buy, qty: crate::types::QTY_SCALE,
        kind: K::Limit { price: 100 * P }, tif: b'0', attrs: allocation("50"),
    });
    assert_eq!(
        spread(&placed),
        vec![
            (6160, "AllAccounts".to_string()),
            (6159, "EqualQuantity".to_string()),
            (6164, "50".to_string()),
        ],
        "the placement states the allocation, in that order: {placed}",
    );

    let moved = send(&mut context, crate::types::OrderRequest::Modify {
        order_id: 31, price: 101 * P, qty: crate::types::QTY_SCALE, outside_rth: false,
        ord_type: 0, tif: 0, stop_price: 0,
        spec: None,
    });
    assert_eq!(spread(&moved), spread(&placed), "and the replacement restates it: {moved}");

    // The method needs no share, and a share the caller did not state is not
    // sent as an empty one.
    let mut fields: Vec<(u32, String)> = Vec::new();
    push_order_attrs(
        &mut fields, &allocation(""), &K::Limit { price: 100 * P }, Side::Buy, String::new(),
    );
    assert!(
        !fields.iter().any(|(t, _)| *t == 6164),
        "a blank share is left off, not written blank: {fields:?}",
    );
}


/// A preview is the order it asks about, and nothing less.
///
/// It used to be encoded from the order type alone: tag 40 from a byte, tag 44
/// for anything that was not a market order, no execution instruction at all.
/// So a preview of an on-close, market-to-limit, market-with-protection or box
/// top order stated a limit price on a type that carries none and came back
/// "Invalid value in field # 44"; a limit-if-touched lost its trigger and came
/// back "Message must contain field # 99"; a trailing stop with a limit lost
/// its trail and came back "Message must contain field # 211"; and every
/// trailing, relative and pegged order lost the one character that tells the
/// four of them apart under the name they share, and came back "Invalid value
/// in field # 18".
#[test]
fn a_preview_states_everything_the_order_states() {
    use crate::types::OrderKind as K;
    let scale = crate::types::PRICE_SCALE;
    let kinds = [
        K::Market,
        K::Limit { price: 100 * scale },
        K::Moc,
        K::Mtl,
        K::MktPrt,
        K::Lit { price: 100 * scale, stop_price: 99 * scale },
        K::TrailingStop { trail_amt: scale, trail_stop_price: 99 * scale },
        K::TrailPct { trail_pct: 100, trail_stop_price: 99 * scale },
        K::TrailingStopLimit { lmt_offset: scale, trail_amt: scale, trail_stop_price: 99 * scale },
        K::Rel { offset: scale / 100 },
        K::PassiveRel { offset: scale / 100, price_cap: 0 },
        K::PegBest { price: 100 * scale },
        K::PegMkt { offset: scale / 100, price_cap: 0 },
        K::PegMid { offset: scale / 100, price_cap: 0 },
        K::StpPrt { stop_price: 99 * scale },
        K::SnapMid { offset: scale / 100 },
    ];
    // The sending time is stamped per message, so the two differ in tag 52 and
    // 60 whatever else happens; everything the order describes is compared.
    let described = |msg: &str| -> Vec<String> {
        msg.split('\u{1}')
            .filter(|f| !f.starts_with("52=") && !f.starts_with("60=")
                && !f.starts_with("9=") && !f.starts_with("10=") && !f.starts_with("6091="))
            .map(str::to_string)
            .collect()
    };
    for kind in kinds {
        let placed = send_kind_for_test(kind.clone(), b'0', crate::types::OrderAttrs::default());
        let preview = send_kind_for_test(
            kind.clone(),
            b'0',
            crate::types::OrderAttrs { what_if: true, ..Default::default() },
        );
        assert_eq!(
            described(&placed),
            described(&preview),
            "{kind:?} is previewed as a different order\nplaced:  {placed}\npreview: {preview}",
        );
        assert!(
            preview.contains("\u{1}6091=1\u{1}"),
            "{kind:?} is previewed without the flag that makes it one: {preview}",
        );
        assert!(
            !placed.contains("\u{1}6091="),
            "{kind:?} is placed carrying the preview flag: {placed}",
        );
    }

    // The four the venue named a field for, stated one at a time so a
    // regression says which one came back.
    let preview = |kind: K| {
        send_kind_for_test(kind, b'0', crate::types::OrderAttrs { what_if: true, ..Default::default() })
    };
    let stated = |msg: &str, t: &str| {
        msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string))
    };
    for (name, msg) in [
        ("MOC", preview(K::Moc)),
        ("MTL", preview(K::Mtl)),
        ("MKT PRT", preview(K::MktPrt)),
    ] {
        assert_eq!(stated(&msg, "44="), None, "{name} carries no limit price: {msg}");
    }
    let lit = preview(K::Lit { price: 100 * scale, stop_price: 99 * scale });
    assert_eq!(stated(&lit, "99=").as_deref(), Some("99"), "a touch price is stated: {lit}");
    let tsl = preview(K::TrailingStopLimit {
        lmt_offset: scale, trail_amt: 2 * scale, trail_stop_price: 99 * scale,
    });
    assert_eq!(stated(&tsl, "211=").as_deref(), Some("2"), "a trail is stated: {tsl}");
    for (name, kind, inst) in [
        ("a trailing stop", K::TrailingStop { trail_amt: scale, trail_stop_price: 0 }, "a"),
        ("a relative order", K::Rel { offset: scale / 100 }, "R"),
        ("a market peg", K::PegMkt { offset: scale / 100, price_cap: 0 }, "P"),
        ("a midpoint peg", K::PegMid { offset: scale / 100, price_cap: 0 }, "M"),
    ] {
        let msg = preview(kind);
        assert_eq!(
            stated(&msg, "18=").as_deref(), Some(inst),
            "{name} is previewed without the instruction that names it: {msg}",
        );
    }
}

/// A passive relative order pegs under a name of its own. The offset rides
/// the peg tag the way a relative order's does, and no instruction rides
/// tag 18 beside it: the name is what tells the venue which peg this is.
#[test]
fn a_passive_relative_order_states_its_offset_under_its_own_name() {
    use crate::types::OrderKind as K;
    let scale = crate::types::PRICE_SCALE;
    let stated = |msg: &str, t: &str| {
        msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string))
    };

    let msg = send_kind_for_test(
        K::PassiveRel { offset: scale / 100, price_cap: 0 },
        b'0',
        crate::types::OrderAttrs::default(),
    );
    assert_eq!(stated(&msg, "40=").as_deref(), Some("PSVR"), "its own name: {msg}");
    assert_eq!(stated(&msg, "211=").as_deref(), Some("0.01"), "the offset is stated: {msg}");
    assert_eq!(stated(&msg, "44="), None, "no cap stated, none sent: {msg}");
    assert_eq!(stated(&msg, "18="), None, "no instruction beside the name: {msg}");
}

/// The cap a passive relative order states rides the limit-price tag, the
/// way the venue's own shape for the type carries it.
#[test]
fn a_passive_relative_order_states_its_cap_on_the_limit_price() {
    use crate::types::OrderKind as K;
    let scale = crate::types::PRICE_SCALE;
    let stated = |msg: &str, t: &str| {
        msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string))
    };

    let msg = send_kind_for_test(
        K::PassiveRel { offset: scale / 100, price_cap: 100 * scale },
        b'0',
        crate::types::OrderAttrs::default(),
    );
    assert_eq!(stated(&msg, "40=").as_deref(), Some("PSVR"), "its own name: {msg}");
    assert_eq!(stated(&msg, "211=").as_deref(), Some("0.01"), "the offset is stated: {msg}");
    assert_eq!(stated(&msg, "44=").as_deref(), Some("100"), "the cap is stated: {msg}");
}

/// A pegged-to-best order states its price on the limit-price tag under its
/// own name, and nothing beside it: no peg offset, no instruction.
#[test]
fn a_peg_best_order_states_its_price_under_its_own_name() {
    use crate::types::OrderKind as K;
    let scale = crate::types::PRICE_SCALE;
    let stated = |msg: &str, t: &str| {
        msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string))
    };

    let msg = send_kind_for_test(
        K::PegBest { price: 100 * scale },
        b'0',
        crate::types::OrderAttrs::default(),
    );
    assert_eq!(stated(&msg, "40=").as_deref(), Some("E2M"), "its own name: {msg}");
    assert_eq!(stated(&msg, "44=").as_deref(), Some("100"), "the price is stated: {msg}");
    assert_eq!(stated(&msg, "211="), None, "no peg offset to state: {msg}");
    assert_eq!(stated(&msg, "18="), None, "no instruction beside the name: {msg}");
}

/// What a caller sets on a pegged-to-best order beside its price reaches the
/// wire: the smallest size worth competing for, the smallest size to compete
/// against, how far past the best to compete, and whether the order is held
/// by the desk rather than the book.
#[test]
fn a_peg_best_order_carries_what_competing_needs() {
    use crate::types::OrderKind as K;
    let scale = crate::types::PRICE_SCALE;
    let stated = |msg: &str, t: &str| {
        msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string))
    };

    let msg = send_kind_for_test(
        K::PegBest { price: 100 * scale },
        b'0',
        crate::types::OrderAttrs {
            min_trade_qty: 100,
            min_compete_size: 500,
            compete_against_best_offset: 0.01,
            not_held: true,
            ..Default::default()
        },
    );
    assert_eq!(stated(&msg, "8415=").as_deref(), Some("100"), "the minimum trade size: {msg}");
    assert_eq!(stated(&msg, "8411=").as_deref(), Some("500"), "the minimum compete size: {msg}");
    assert_eq!(stated(&msg, "8412=").as_deref(), Some("0.010000"), "how far past the best: {msg}");
    assert_eq!(stated(&msg, "6287=").as_deref(), Some("1"), "held by the desk: {msg}");
    assert_eq!(stated(&msg, "8403="), None, "no mid offset where none was stated: {msg}");
    assert_eq!(stated(&msg, "8404="), None, "no mid offset where none was stated: {msg}");
}

/// Stated up to the midpoint rather than against the best price, a
/// pegged-to-best order carries the two mid offsets and no compete offset.
#[test]
fn a_peg_best_order_up_to_the_mid_states_its_mid_offsets() {
    use crate::types::OrderKind as K;
    let scale = crate::types::PRICE_SCALE;
    let stated = |msg: &str, t: &str| {
        msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string))
    };

    let msg = send_kind_for_test(
        K::PegBest { price: 100 * scale },
        b'0',
        crate::types::OrderAttrs {
            mid_offset_at_whole: 0.02,
            mid_offset_at_half: 0.01,
            ..Default::default()
        },
    );
    assert_eq!(stated(&msg, "8403=").as_deref(), Some("0.020000"), "the offset at the whole spread: {msg}");
    assert_eq!(stated(&msg, "8404=").as_deref(), Some("0.010000"), "the offset at half the spread: {msg}");
    assert_eq!(stated(&msg, "8412="), None, "no compete offset beside them: {msg}");
}

/// A withdrawal of the whole account that never left this client is said to
/// the caller, not only written to the log.
///
/// It names no order, so there is no order to report it against and nothing
/// on the order callback carries it. Both surfaces had already returned
/// success and given the caller nothing afterwards, so a kill switch that
/// never fired read exactly like one that did.
#[test]
fn a_withdrawal_of_everything_that_never_went_is_said_to_the_caller() {
    let mut context = Context::new();
    let shared = Arc::new(SharedState::new());
    // One per instrument, which is how a caller asking for everything back
    // reaches the engine.
    context.pending_orders.push(OrderRequest::CancelAll { instrument: 0 });
    context.pending_orders.push(OrderRequest::CancelAll { instrument: 1 });

    refuse_what_is_left(
        &mut context, &shared, "recovery of the trading connection was given up",
    );

    let told = shared.reference.drain_historical_errors();
    assert_eq!(
        told.len(), 1,
        "one refusal for the withdrawal, not one per instrument: {told:?}",
    );
    let (req_id, code, message) = &told[0];
    assert_eq!(
        *req_id, crate::bridge::ReferenceState::NO_REQUEST,
        "against no request of the caller's, which is what it asked under",
    );
    assert_eq!(*code, crate::error_codes::Refusal::NOT_CONNECTED);
    assert!(
        message.contains("withdrawal of every order"),
        "and it says what did not reach the venue: {message}",
    );
}

/// A replacement is named `orderId.revision`, and the revision counts up.
///
/// The counter has a width. Reached, the next increment overflows: the engine
/// dies where overflow is checked, and where it is not the name wraps to a
/// revision the venue has already answered, so the order is replaced under an
/// older name than the one it is working under. Either way the record had
/// already been overwritten with the attempted terms, so the caller reads an
/// order the venue never took.
#[test]
fn a_replacement_that_cannot_be_named_leaves_the_order_standing() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy,
        100 * crate::types::QTY_SCALE, 150 * crate::types::PRICE_SCALE,
        b'2', b'1', 0,
    ));
    // The last name the order can carry.
    context.modify_versions.insert(42, u32::MAX);
    context.last_clord.insert(42, format!("42.{}", u32::MAX));
    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 42,
        price: 151 * crate::types::PRICE_SCALE,
        qty: 100 * crate::types::QTY_SCALE,
        outside_rth: false,
        ord_type: 0,
        tif: 0,
        stop_price: 0,
        spec: None,
    });

    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);

    let mut buf = [0u8; 8192];
    assert!(peer.read(&mut buf).is_err(), "nothing went out under a name the venue has answered");
    let kept = context.order(42).expect("the order the venue holds is still tracked");
    assert_eq!(kept.price, 150 * crate::types::PRICE_SCALE, "on the terms the venue holds");
    assert_ne!(
        kept.status, crate::types::OrderStatus::PendingReplace,
        "and not standing as though a replace were out for it",
    );
    assert_eq!(
        context.modify_versions.get(&42), Some(&u32::MAX),
        "the counter did not wrap past its last name",
    );
    let said = shared.orders.drain_order_inactive();
    assert!(
        said.iter().any(|(id, _, why)| *id == 42 && why.contains("named")),
        "and the caller is told why the replace did not go: {said:?}",
    );
    // And on the channel a refusal travels on, so the record the surfaces
    // read goes back to the terms the venue holds. Said only in the message
    // above, they kept the terms of an attempt that never left this process.
    let refusals = shared.orders.drain_cancel_rejects();
    assert!(
        refusals.iter().any(|r| r.order_id == 42 && r.reject_type == 2),
        "the surfaces are told the change did not go: {refusals:?}",
    );
}

/// A cancel that does not reach the wire leaves an outstanding change alone.
///
/// A write that fails rolls back the attempt it wrote, and a cancel writes
/// none — but it names an order that is in the book, which is why it is being
/// cancelled, so the rollback took it for a failed replace. It threw away what
/// the replacement still outstanding falls back to, and the venue's answer to
/// that replacement, either way, then read as belonging to nothing: a refusal
/// put nothing back, and an acceptance settled nothing.
#[test]
fn a_cancel_that_does_not_go_leaves_the_change_outstanding() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.insert_order(crate::types::Order::new(
        42, instrument, Side::Buy,
        100 * crate::types::QTY_SCALE, 150 * crate::types::PRICE_SCALE,
        b'2', b'0', 0,
    ));
    // A change is out, and what the venue holds is kept under its revision.
    let before = *context.order(42).expect("tracked");
    context.modify_versions.insert(42, 1);
    context.pre_replace.insert((42, 1), (before, "42.0".to_string(), None));

    context.pending_orders.push(crate::types::OrderRequest::Cancel { order_id: 42 });
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);

    assert!(
        context.pre_replace.contains_key(&(42, 1)),
        "the change still outstanding keeps what it falls back to",
    );
}

/// A request that names a slot and puts nothing in the book offers it back
/// when it drains.
///
/// A submit is in the book from the moment it is built, so retiring it offers
/// the slot back. A cancel-all names a slot and puts nothing in it, so once it
/// has drained nothing would ever ask about that slot again. Asked from the
/// reclaim instead, the question was put once per lap for as long as the
/// request sat in the buffer.
#[test]
fn a_request_that_names_a_slot_offers_it_back_when_it_drains() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.pending_orders.push(crate::types::OrderRequest::CancelAll { instrument });

    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);

    assert_eq!(
        context.slots_to_reconsider, vec![instrument],
        "the slot it named is offered back once, as it leaves the buffer",
    );
}

/// A placement does not ask the loop to reconsider the slot it is on.
///
/// It is in the book by the time the loop looks, so the reclaim's own guard
/// refuses it — and the guard is a scan of every open order. Asked for every
/// request that names a slot, a session placing orders enqueued one such scan
/// per order placed, on exactly the workload the reclaiming exists for.
#[test]
fn a_placement_does_not_ask_for_its_slot_back() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.pending_orders.push(crate::types::OrderRequest::SubmitEx { con_id: 0,
        order_id: 42,
        instrument,
        side: Side::Buy,
        qty: 100 * crate::types::QTY_SCALE,
        kind: crate::types::OrderKind::Limit { price: 150 * crate::types::PRICE_SCALE },
        tif: b'0',
        attrs: Default::default(),
    });

    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);

    assert!(context.order(42).is_some(), "the order is in the book");
    assert!(
        context.slots_to_reconsider.is_empty(),
        "and nothing asks the loop to scan the book for a slot it is holding",
    );
}

/// An order naming a contract the slot no longer holds is not sent.
///
/// A slot is an index, and a freed one goes to the next contract that needs
/// it. A caller reads which slot a contract holds from a cache, and between
/// that read and the request reaching the engine the slot can have been given
/// away — so the order was built from whatever holds it now: another
/// contract's symbol, another contract's routing, and a fill that would move
/// that contract's position.
#[test]
fn an_order_whose_slot_changed_hands_is_not_sent() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    // The slot the caller read, holding a different contract by the time the
    // request is built.
    let instrument = context.register_instrument(320227571);
    context.set_symbol(instrument, "QQQ".to_string());
    context.market.set_routing(instrument, "STK", "SMART");
    context.pending_orders.push(crate::types::OrderRequest::SubmitEx {
        order_id: 42,
        instrument,
        con_id: 756733,
        side: Side::Buy,
        qty: 100 * crate::types::QTY_SCALE,
        kind: crate::types::OrderKind::Limit { price: 150 * crate::types::PRICE_SCALE },
        tif: b'0',
        attrs: Default::default(),
    });

    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);

    let mut buf = [0u8; 8192];
    assert!(peer.read(&mut buf).is_err(), "nothing went out on the wrong contract");
    let said = shared.orders.drain_order_inactive();
    assert!(
        said.iter().any(|(id, _, why)| *id == 42
            && why.contains("756733") && why.contains("320227571")),
        "and the caller is told which contract it named and which the place now holds: {said:?}",
    );
}

/// A bracket leg replaced keeps its group and its parent.
///
/// The bracket's three legs went out as raw messages and none was recorded as
/// placed. A replace is a full statement of the order and restates from that
/// record, so a leg's replace carried no group and no parent: the venue took
/// the replacement as stated and the leg left its bracket -- a stop-loss that
/// no longer cancels when its take-profit fills.
#[test]
fn a_replaced_bracket_leg_keeps_its_group_and_its_parent() {
    use std::io::Read;
    let (conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut conn = Some(conn);
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.set_routing(instrument, "STK", "SMART");
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());

    context.pending_orders.push(crate::types::OrderRequest::SubmitBracket {
        con_id: 756733, parent_id: 10, tp_id: 11, sl_id: 12, instrument,
        side: Side::Buy, qty: 100 * crate::types::QTY_SCALE,
        entry_price: 150 * crate::types::PRICE_SCALE,
        take_profit: 155 * crate::types::PRICE_SCALE,
        stop_loss: 145 * crate::types::PRICE_SCALE,
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    // The three legs, read and set aside.
    let mut buf = vec![0u8; 65536];
    let n = peer.read(&mut buf).unwrap();
    let placed = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");
    assert!(placed.contains("|583=OCA_10|"), "the legs were placed in a group: {placed}");

    // The stop-loss is moved.
    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 12, price: 0,
        qty: 100 * crate::types::QTY_SCALE, outside_rth: false,
        ord_type: 0, tif: 0, stop_price: 144 * crate::types::PRICE_SCALE,
        spec: None,
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let n = peer.read(&mut buf).unwrap();
    let replaced = String::from_utf8_lossy(&buf[..n]).replace('\u{1}', "|");

    assert!(replaced.contains("|35=G|"), "a replace went out: {replaced}");
    assert!(
        replaced.contains("|583=OCA_10|"),
        "the replacement restates the group the leg was placed in: {replaced}",
    );
    assert!(
        replaced.contains("|6107=10.0|"),
        "and the parent it was placed under: {replaced}",
    );
}

/// A statement kept for a replace the book cannot take goes with the refusal.
///
/// The statement is the record for the replace behind it and for nothing
/// else. A replace of an order the book does not hold is refused, and a
/// statement left standing for it grew the record by one order for every
/// such id, the growth the retirement of orders exists to stop.
#[test]
fn a_statement_for_a_refused_replace_goes_with_the_refusal() {
    use crate::types::{OrderKind as K, PRICE_SCALE as P};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    context.register_instrument(756733);
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 77, price: 101 * P, qty: crate::types::QTY_SCALE, outside_rth: false,
        ord_type: 0, tif: 0, stop_price: 0,
        spec: Some(Box::new(crate::types::OrderSpec {
            kind: K::Limit { price: 100 * P },
            attrs: crate::types::OrderAttrs::default(),
        })),
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let refused = shared.orders.drain_order_inactive();
    assert!(refused.iter().any(|(id, ..)| *id == 77), "the replace is refused: {refused:?}");
    assert!(
        !context.submitted.contains_key(&77),
        "and the statement it carried left no record of an order that does not exist",
    );
}

/// A replace changes the order the venue is working; it does not restate it
/// from nothing.
///
/// What the caller states is merged onto the terms the venue already holds, so
/// an attribute the caller says nothing about keeps the value it has. A caller
/// who repriced an order and stated no reference for it had their own name for
/// that order cleared, because the statement was taken whole.
///
/// Three attributes a replace cannot change at all — the group it is in, its
/// permission to trade outside the session, and its permission to join the
/// pre-open auction. The venue answers a replace that tries to change them by
/// naming them back, not by applying them.
#[test]
fn a_replace_merges_onto_the_order_the_venue_holds() {
    use std::io::Read;
    use crate::types::{OrderKind as K, PRICE_SCALE as P};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(300))).unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());
    let mut buf = [0u8; 8192];

    // Placed naming itself, in a group, and allowed outside the session.
    context.pending_orders.push(crate::types::OrderRequest::SubmitEx {
        con_id: 0, order_id: 91, instrument, side: Side::Buy, qty: crate::types::QTY_SCALE,
        kind: K::Limit { price: 100 * P }, tif: b'0',
        attrs: crate::types::OrderAttrs {
            order_ref: "KEEP".to_string(),
            oca_group_str: "G1".to_string(),
            outside_rth: true,
            ..Default::default()
        },
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let n = peer.read(&mut buf).unwrap_or(0);
    let placed = String::from_utf8_lossy(&buf[..n]).to_string();
    assert_eq!(stated(&placed, "6010=").first().map(String::as_str), Some("KEEP"), "{placed}");

    // Repriced, stating no reference, no group and no permission: each keeps
    // what the order already carries.
    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 91, price: 101 * P, qty: crate::types::QTY_SCALE, outside_rth: false,
        ord_type: 0, tif: 0, stop_price: 0,
        spec: Some(Box::new(crate::types::OrderSpec {
            kind: K::Limit { price: 101 * P },
            attrs: crate::types::OrderAttrs { display_size: 25, ..Default::default() },
        })),
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let n = peer.read(&mut buf).unwrap_or(0);
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    assert_eq!(
        stated(&msg, "6010=").first().map(String::as_str), Some("KEEP"),
        "the caller's own name for the order survives a replace that states none: {msg}",
    );
    assert_eq!(
        stated(&msg, "583=").first().map(String::as_str), Some("G1"),
        "and the group it is in, which a replace cannot change: {msg}",
    );
    assert_eq!(
        stated(&msg, "111=").first().map(String::as_str), Some("25"),
        "while what the caller did state reaches the venue: {msg}",
    );
}

/// A replace names where the order is working, not where the contract is
/// watched now.
///
/// The destination is restated on every replace and was read from the slot the
/// order sits in — and a subscription on the same contract writes that slot's
/// routing too. A caller directing an order to one venue and then watching the
/// contract on another had the routing moved under the resting order, so its
/// next replace named a venue the order was never working on. What it went out
/// on is kept beside what it was submitted as, and read back here.
#[test]
fn a_replace_names_the_venue_the_order_went_out_on() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    // Directed: the caller named the venue on the contract it ordered against.
    context.market.set_routing(instrument, "CS", "NYSE");
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());

    context.pending_orders.push(crate::types::OrderRequest::SubmitEx {
        con_id: 0,
        order_id: 42,
        instrument,
        side: Side::Buy,
        qty: 100 * crate::types::QTY_SCALE,
        kind: crate::types::OrderKind::Limit { price: 400 * crate::types::PRICE_SCALE },
        tif: b'0',
        attrs: Default::default(),
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let mut buf = [0u8; 8192];
    let n = peer.read(&mut buf).unwrap();
    let placed = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(placed.contains("\u{1}100=NYSE\u{1}"), "it went out to NYSE: {placed}");

    // And then the contract is watched somewhere else, which writes the slot.
    context.market.set_routing(instrument, "CS", "SMART");

    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 42,
        price: 401 * crate::types::PRICE_SCALE,
        qty: 100 * crate::types::QTY_SCALE,
        outside_rth: false,
        ord_type: 0,
        tif: 0,
        stop_price: 0,
        spec: None,
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("35=").as_deref(), Some("G"), "a replace was sent: {msg}");
    assert_eq!(
        tag("100=").as_deref(), Some("NYSE"),
        "the replace named where the contract is watched now, not where the \
         order is working: {msg}",
    );
    assert_eq!(tag("6210=").as_deref(), Some("NYSE"), "and its second statement too: {msg}");
}

/// And a bracket's legs keep the venue they were sent to, as a single order
/// does.
///
/// The three legs go out on one instruction, and each is replaceable on its
/// own afterwards. Recorded for the single submit and not for the bracket, a
/// leg replaced after a watch was opened on the contract named somewhere it had
/// never been working — the same defect the single order had, on the path that
/// places three at a time.
#[test]
fn a_bracket_leg_is_replaced_on_the_venue_it_went_out_to() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let mut conn = Some(crate::protocol::connection::Connection::new_raw(stream).unwrap());
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    context.market.set_routing(instrument, "CS", "NYSE");
    let mut hb = crate::engine::hot_loop::HeartbeatState::new();
    let shared = std::sync::Arc::new(SharedState::new());

    context.pending_orders.push(crate::types::OrderRequest::SubmitBracket {
        con_id: 0,
        parent_id: 60,
        tp_id: 61,
        sl_id: 62,
        instrument,
        side: Side::Buy,
        qty: crate::types::QTY_SCALE,
        entry_price: 400 * crate::types::PRICE_SCALE,
        take_profit: 410 * crate::types::PRICE_SCALE,
        stop_loss: 390 * crate::types::PRICE_SCALE,
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let mut buf = [0u8; 16384];
    let n = peer.read(&mut buf).unwrap();
    let placed = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(placed.contains("\u{1}100=NYSE\u{1}"), "the legs went out to NYSE: {placed}");

    // The contract is then watched somewhere else, which writes the slot.
    context.market.set_routing(instrument, "CS", "SMART");

    // The take-profit leg is replaced on its own.
    context.pending_orders.push(crate::types::OrderRequest::Modify {
        order_id: 61,
        price: 411 * crate::types::PRICE_SCALE,
        qty: crate::types::QTY_SCALE,
        outside_rth: false, ord_type: 0, tif: 0, stop_price: 0,
        spec: None,
    });
    drain_and_send_orders(&mut conn, &mut context, "DU1", &mut hb, false, &shared, false, &None);
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    let tag = |t: &str| msg.split('\u{1}').find_map(|f| f.strip_prefix(t).map(str::to_string));
    assert_eq!(tag("35=").as_deref(), Some("G"), "a replace was sent: {msg}");
    assert_eq!(
        tag("100=").as_deref(), Some("NYSE"),
        "the leg's replace named where the contract is watched now, not where the \
         leg is working: {msg}",
    );
}

/// Day-till-cancelled goes out as a good-till-cancelled order carrying the
/// flag that stands it down at the close.
///
/// It shared the good-till-date byte, so the order the venue worked was one
/// with an expiry the caller never named — or, with no date stated beside it,
/// none at all. Either way it rested on past the close it was meant to end at.
#[test]
fn a_day_till_cancelled_order_states_gtc_and_the_flag_that_ends_it() {
    use std::io::Read;
    let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let asked = crate::api::Order { tif: "DTC".to_string(), ..Default::default() };
    send_order_ex(
        &mut conn, &mut context, &shared_for_test(), "DU123456", 78, instrument, Side::Buy, 1,
        crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
        asked.tif_byte(), &asked.attrs(),
    )
    .unwrap();
    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]);
    let f: Vec<&str> = msg.split('\u{1}').collect();
    assert!(f.contains(&"59=1"), "the life it goes out under: {msg}");
    assert!(f.contains(&"6436=1"), "and the flag that ends it at the close: {msg}");
    assert!(!f.contains(&"59=6"), "not a good-till-date order: {msg}");
    assert!(
        !f.iter().any(|field| field.starts_with("432=") || field.starts_with("126=")),
        "and no expiry the caller never named: {msg}",
    );
}

/// An order placed for a model names the model beside the account, on the
/// order and on the cancel that follows it.
///
/// The account says where the order goes; this says which sleeve of it. Sent
/// without it the order trades the account at large, which for a model
/// portfolio is a different position from the one asked for.
#[test]
fn an_order_for_a_model_names_it_on_the_order_and_on_the_cancel() {
    use std::io::Read;
    let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let asked = crate::api::Order { model_code: "GROWTH".to_string(), ..Default::default() };
    send_order_ex(
        &mut conn, &mut context, &shared_for_test(), "DU123456", 79, instrument, Side::Buy, 1,
        crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
        b'0', &asked.attrs(),
    )
    .unwrap();
    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(
        msg.split('\u{1}').any(|field| field == "6700=GROWTH"),
        "the model the order trades against: {msg}",
    );

    send_cancel(&mut conn, &mut context, "DU123456", 79).unwrap();
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(
        msg.split('\u{1}').any(|field| field == "6700=GROWTH"),
        "and the cancel names it too: {msg}",
    );
}

/// The default sleeve is not a model, and naming it states nothing.
///
/// The venue's own gate on this tag is a non-empty test that also excludes the
/// default sleeve by name. An order that named it and stated the tag anyway put
/// a field on the wire that the venue is never sent.
#[test]
fn an_order_for_the_default_sleeve_names_no_model() {
    use std::io::Read;
    let (mut conn, mut peer) = crate::protocol::connection::Connection::for_test();
    let mut context = Context::new();
    let instrument = context.register_instrument(756733);
    context.set_symbol(instrument, "SPY".to_string());
    let asked = crate::api::Order { model_code: "Core".to_string(), ..Default::default() };
    send_order_ex(
        &mut conn, &mut context, &shared_for_test(), "DU123456", 80, instrument, Side::Buy, 1,
        crate::types::OrderKind::Limit { price: crate::types::PRICE_SCALE },
        b'0', &asked.attrs(),
    )
    .unwrap();
    let mut buf = [0u8; 4096];
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(
        !msg.split('\u{1}').any(|field| field.starts_with("6700=")),
        "no model tag at all: {msg}",
    );

    send_cancel(&mut conn, &mut context, "DU123456", 80).unwrap();
    let n = peer.read(&mut buf).unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(
        !msg.split('\u{1}').any(|field| field.starts_with("6700=")),
        "and the cancel states none either: {msg}",
    );
}
