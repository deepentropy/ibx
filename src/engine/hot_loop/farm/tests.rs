//! The tests for this module.
//!
//! One file per module, as `api/client` already does it. Each block below
//! reaches the code it tests through `super::super`, which is the module this
//! file belongs to.

use super::*;

/// Every inner message the peer has been sent, decompressed, in order.
pub(crate) fn drain_inner(peer: &mut Connection) -> Vec<Vec<u8>> {
    let mut inner = Vec::new();
    loop {
        match peer.try_recv() {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        for frame in peer.extract_frames() {
            let Frame::FixComp(raw) = frame else { continue };
            let Some(unsigned) = peer.unsign(&raw) else { continue };
            inner.extend(fixcomp::fixcomp_decompress(&unsigned).unwrap_or_default());
        }
    }
    inner
}

/// Holdings and figures arrive on the trading connection's download. The
/// market-data connection's copy of the same handlers struck a holding from
/// the set a rebuilt download was still restating, so a holding the account
/// closed while the connection was down survived the squaring. A position
/// frame on this connection is recorded as unread and touches nothing.
#[test]
fn a_position_frame_on_the_market_data_connection_is_recorded_not_applied() {
    let mut farm = FarmState::new();
    let mut context = crate::engine::context::Context::new();
    let shared = crate::bridge::SharedState::new();
    let msg = b"8=FIX.4.1\x0135=UP\x016008=756733\x016064=100\x016068=SPY\x01";
    farm.process_farm_message(msg, &mut None, &mut context, &shared, &None, &mut HeartbeatState::new());
    assert!(shared.portfolio.position_info(756733).is_none(), "nothing is applied");
    assert!(
        shared.market.unread_wire().iter().any(|(_, what)| what == "type UP"),
        "and the frame is recorded as unread: {:?}",
        shared.market.unread_wire(),
    );
}

mod news_tests {
    use super::super::*;
    use crate::bridge::SharedState;
    use crate::engine::context::Context;

    /// Frame records the way the venue frames them: the length of everything
    /// after it in bits, then each record as its own tick states lengths.
    fn framed_generic_ticks(records: &[(u32, u32, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (server_tag, tick, payload) in records {
            body.extend_from_slice(&server_tag.to_be_bytes());
            match PayloadLength::of(*tick) {
                PayloadLength::OneByte => body.push(payload.len() as u8),
                PayloadLength::TwoBytes => {
                    body.extend_from_slice(&(payload.len() as u16).to_be_bytes())
                }
                PayloadLength::ToTheEnd => {}
            }
            body.extend_from_slice(payload);
        }
        let mut msg = b"35=G\x01".to_vec();
        msg.extend_from_slice(&(((body.len() * 8) % 65_536) as u16).to_be_bytes());
        msg.extend_from_slice(&body);
        msg
    }

    /// One news record.
    fn framed_news(server_tag: u32, payload: &[u8]) -> Vec<u8> {
        framed_generic_ticks(&[(server_tag, NEWS_REQUEST_TYPE, payload)])
    }

    /// One article, laid out as the handler reads it.
    fn one_article() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&4u32.to_be_bytes());
        body.extend_from_slice(b"BRFG");
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&2u16.to_be_bytes());
        body.extend_from_slice(b"id");
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&1_785_325_554u32.to_be_bytes());
        body.extend_from_slice(&8u32.to_be_bytes());
        body.extend_from_slice(b"headline");
        body
    }

    /// A news subscription acknowledged by the ticker setup keyed to the
    /// contract, rather than under its own number, files its tag there.
    #[test]
    fn a_news_subscription_files_its_tag_on_the_ticker_setup_too() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        farm.send_news_subscribe(756733, instrument, "STK", "BRFG", 7, &mut None, &mut HeartbeatState::new());

        farm.handle_ticker_setup(b"35=L\x01756733,0.01,44011", &mut context, &shared);
        farm.handle_generic_tick(&framed_news(44011, &one_article()), &mut context, &shared, &None);
        assert_eq!(shared.market.drain_tick_news().len(), 1, "the headline reaches the caller");
    }

    /// A news subscribe whose write the socket refused keeps its entry for the
    /// reconnect rebuild, rather than dropping it and refusing under an
    /// internal request id the caller never issued. The write failing means the
    /// socket is going; the drop's 2103 tells the caller and the rebuild
    /// re-sends from `news_subscriptions`.
    #[test]
    fn a_news_subscribe_write_failure_keeps_the_entry_for_the_rebuild() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let instrument = context.market.register(756733);
        let (mut conn, _peer) = Connection::for_test();
        conn.fail_writes();
        let mut conn = Some(conn);

        farm.send_news_subscribe(756733, instrument, "STK", "BRFG", 7, &mut conn, &mut HeartbeatState::new());

        assert_eq!(
            farm.news_subscriptions.len(), 1,
            "the subscription is kept for the reconnect rebuild when the write fails",
        );
    }

    /// Forgotten, a news subscription's tag goes with it, and the request
    /// with it survives neither: a headline arriving after is nobody's.
    #[test]
    fn a_forgotten_news_subscription_leaves_no_tag_behind() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        farm.send_news_subscribe(756733, instrument, "STK", "BRFG", 7, &mut None, &mut HeartbeatState::new());
        farm.handle_subscription_ack(b"35=Q\x0133082,7,0.01,0,3", &mut context, &shared);
        farm.forget_news(7, instrument);

        farm.handle_generic_tick(&framed_news(33082, &one_article()), &mut context, &shared, &None);
        assert!(shared.market.drain_tick_news().is_empty(), "nothing after the withdrawal");
        assert!(farm.generic_tick_reqs.iter().all(|(rid, _)| *rid != 7));
    }

    /// News is asked for under its own request and withdrawn under its own, so
    /// withdrawing the quote on the same contract does not take it. Taken with
    /// it, the subscription stands while its headlines arrive under a tag
    /// nothing reads: no rejection, no end, just silence.
    #[test]
    fn withdrawing_the_quote_leaves_the_news_reading() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut None, &mut hb,
        );
        farm.send_news_subscribe(756733, instrument, "STK", "BRFG", 7, &mut None, &mut hb);
        farm.handle_subscription_ack(b"35=Q\x0133082,7,0.01,0,3", &mut context, &shared);

        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], u64::MAX, false, &mut None, &mut hb);

        farm.handle_generic_tick(&framed_news(33082, &one_article()), &mut context, &shared, &None);
        assert_eq!(
            shared.market.drain_tick_news().len(), 1,
            "the news subscription stands, so its headline still reaches the caller",
        );
    }

    /// A series the caller named is asked for, under the venue's own number
    /// for it.
    ///
    /// The number a caller states in the generic tick list is the number the
    /// venue knows the series by, so there is nothing to translate: the series
    /// goes out as a subscription of its own carrying that number, beside the
    /// prices rather than instead of them. This client used to accept the list
    /// and send nothing for it, so a caller asking for the shortable count or
    /// the trade rate waited on a stream that was never asked for.
    #[test]
    fn a_named_series_is_asked_for_under_the_venues_own_number() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        // RTVolume and the shortable count, as a caller names them.
        farm.asked_generic_ticks.insert(instrument, vec![233, 236]);
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );

        let stated = |msg: &[u8], tag: u32| -> Vec<String> {
            let prefix = format!("{tag}=");
            msg.split(|&b| b == 0x01)
                .filter_map(|field| {
                    std::str::from_utf8(field).ok()?.strip_prefix(prefix.as_str()).map(str::to_string)
                })
                .collect()
        };
        // One request carries the prices and the named series together, and
        // states how many entries it carries.
        let mut carried = None;
        for msg in super::drain_inner(&mut peer) {
            if stated(&msg, 263).first().map(String::as_str) == Some("1")
                && stated(&msg, 264).iter().any(|t| t == "442")
            {
                carried = Some((stated(&msg, 264), stated(&msg, 146), stated(&msg, 262)));
            }
        }
        let (types, count, numbers) = carried.expect("the subscription goes out");
        for tick in ["233", "236"] {
            assert!(
                types.iter().any(|t| t == tick),
                "the series the caller named rides on the same request: {types:?}",
            );
        }
        assert_eq!(
            count.first().map(String::as_str), Some("4"),
            "and the count ahead of them is every entry: {types:?}",
        );
        assert_eq!(numbers.len(), 4, "each entry under a number of its own: {numbers:?}");
        // And each under a request of its own, so what comes back can be told
        // apart from the prices and from the other series.
        for tick in [233u32, 236] {
            assert!(
                farm.generic_tick_reqs.iter().any(|(_, kind)| *kind == tick),
                "a request of its own is recorded for {tick}: {:?}", farm.generic_tick_reqs,
            );
        }

        // And each is an entry of the subscription, because the withdrawal is
        // composed from those: left out of them, a series was never withdrawn
        // and its number outlived the slot it was asked on.
        let record = farm.instrument_md_reqs.iter()
            .find(|(id, _)| *id == instrument)
            .map(|(_, record)| record)
            .expect("the subscription is recorded");
        for tick in [233u32, 236] {
            assert!(
                record.entries.iter().any(|e| e.request_type == tick),
                "the series is an entry of the subscription: {:?}",
                record.entries.iter().map(|e| e.request_type).collect::<Vec<_>>(),
            );
        }

        // A connection dying does not forget what the caller asked for: the
        // rebuild behind it reads the list from here, and nothing else in the
        // process holds it.
        let mut farm_after = FarmState::new();
        farm_after.asked_generic_ticks.insert(instrument, vec![233, 236]);
        farm_after.handle_disconnect(&mut None, &mut context, &None, &SharedState::new());
        assert_eq!(
            farm_after.asked_generic_ticks.get(&instrument).map(Vec::as_slice),
            Some([233u32, 236].as_slice()),
            "the series the caller named survive the connection that carried them",
        );

        // And they go with the subscription rather than outliving it, which is
        // where the caller's asking actually ends.
        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], u64::MAX, false, &mut conn, &mut hb);
        assert!(
            !farm.asked_generic_ticks.contains_key(&instrument),
            "what was asked for on this contract is released with it",
        );
        // The withdrawal states every number the subscription handed out,
        // each one as its own row.
        let withdrawn: Vec<String> = super::drain_inner(&mut peer)
            .into_iter()
            .filter(|msg| stated(msg, 263).first().map(String::as_str) == Some("2"))
            .flat_map(|msg| stated(&msg, 264))
            .collect();
        for tick in ["233", "236"] {
            assert!(
                withdrawn.iter().any(|t| t == tick),
                "the series is withdrawn as itself: {withdrawn:?}",
            );
        }
    }

    /// What the venue states about an issuer is held under the contract, as
    /// the venue's own pairs.
    ///
    /// Three series carry it. One states four bytes of its own in front of
    /// text running to the end of the record, and two state the text's length
    /// as a count, so a reader that treated them alike read one record's
    /// length as part of the next one's first key.
    #[test]
    fn what_the_venue_states_about_an_issuer_is_held_under_the_contract() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);

        // Behind its own count, which is what the record states first.
        let counted = |text: &[u8]| {
            let mut out = (text.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(text);
            out
        };
        let rating = counted(b"RATING=2;ANALYSTS=17");
        farm.generic_tick_tags.push((31, 434, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(31, 434, &rating)]), &mut context, &shared, &None,
        );
        assert_eq!(
            shared.reference.company_data(756733, 434),
            vec![("RATING".to_string(), "2".to_string()),
                 ("ANALYSTS".to_string(), "17".to_string())],
            "the pairs are held as the venue wrote them",
        );

        // The insider record, whose first four bytes are its own.
        let mut insider = vec![0u8, 0, 0, 1];
        insider.extend_from_slice(b"FLOAT=123456789;PCTHELD=61.2");
        farm.generic_tick_tags.push((32, 454, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(32, 454, &insider)]), &mut context, &shared, &None,
        );
        assert_eq!(
            shared.reference.company_data(756733, 454),
            vec![("FLOAT".to_string(), "123456789".to_string()),
                 ("PCTHELD".to_string(), "61.2".to_string())],
            "the four bytes before the text are not read as part of a key",
        );
        assert_eq!(
            shared.reference.company_data_series(756733), vec![434, 454],
            "both series are named as stated",
        );

        // A record this cannot read as text publishes nothing, rather than
        // publishing the byte it could not read as a character the venue
        // never sent.
        farm.generic_tick_tags.push((33, 548, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(33, 548, &counted(b"RATING=\xff"))]),
            &mut context, &shared, &None,
        );
        assert!(
            shared.reference.company_data(756733, 548).is_empty(),
            "a record that is not text was published anyway",
        );

        // Restated, a series replaces what it said rather than adding to it:
        // one message carries the whole set.
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(31, 434, &counted(b"RATING=3"))]),
            &mut context, &shared, &None,
        );
        assert_eq!(
            shared.reference.company_data(756733, 434),
            vec![("RATING".to_string(), "3".to_string())],
            "the later statement stands alone",
        );

        // A count whose low byte is a letter, which is what made this visible
        // on the wire: read as text from the first byte, that letter joins the
        // record's first name and the contract states its fields under names
        // no other contract states them under.
        let mut wide = b"RATING=4;ANALYSTS=9;NOTE=".to_vec();
        while wide.len() < 0x52 {
            wide.push(b'z');
        }
        assert_eq!(wide.len() as u8 as char, 'R', "the count's low byte is a letter");
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(31, 434, &counted(&wide))]), &mut context, &shared, &None,
        );
        assert_eq!(
            shared.reference.company_data(756733, 434).first().map(|(key, _)| key.as_str()),
            Some("RATING"),
            "the count in front of the text was read as part of the first name",
        );
    }

    /// The model the venue works out as a contract closes is kept apart from
    /// the model it is standing behind now.
    ///
    /// Both arrive in the same shape and both are read by the same reader.
    /// Kept in the same place, a figure worked out at yesterday's close would
    /// stand where a caller reads what the contract is worth today.
    #[test]
    fn the_model_at_the_close_does_not_stand_where_the_standing_model_does() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(9005);

        // Valid, with a delta and an underlying price behind the model's own
        // price for the option.
        let model = |opt_price: f64, delta: f64, und: f64| {
            let flags: u32 = 1 | 1 << 16 | 1 << 25;
            let mut out = flags.to_be_bytes().to_vec();
            for value in [opt_price, delta, und] {
                out.extend_from_slice(&value.to_be_bytes());
            }
            out
        };

        farm.generic_tick_tags.push((81, 732, instrument));
        farm.generic_tick_tags.push((82, 733, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[
                (81, 732, &model(26.0, 0.55, 760.0)),
                (82, 733, &model(24.5, 0.51, 755.0)),
            ]),
            &mut context, &shared, &None,
        );

        let standing = shared.market.option_model(instrument).expect("the venue stated one");
        assert_eq!((standing.opt_price, standing.delta), (26.0, 0.55), "{standing:?}");
        let closing =
            shared.market.closing_option_model(instrument).expect("the venue stated one");
        assert_eq!((closing.opt_price, closing.delta), (24.5, 0.51), "{closing:?}");
        assert_eq!(closing.und_price, 755.0, "the fields behind the flags are read the same");

        // A slot handed back takes the closing model with it, as it does the
        // standing one.
        shared.market.forget_option_model(instrument);
        assert!(
            shared.market.closing_option_model(instrument).is_none(),
            "a model outlived the contract it was worked out for",
        );
    }

    /// The two series that state a run of paired figures, one of which states
    /// a version in front of the count and one of which does not.
    ///
    /// Read alike, the versioned one takes its version for the count and its
    /// count for the first half of a pair.
    #[test]
    fn a_series_that_pairs_its_figures_is_read_from_where_its_count_stands() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(9004);

        let pairs = |lead: &[i32], rows: &[(f64, f64)], tail: &[i32]| {
            let mut out = Vec::new();
            for n in lead {
                out.extend_from_slice(&n.to_be_bytes());
            }
            for (first, second) in rows {
                out.extend_from_slice(&first.to_be_bytes());
                out.extend_from_slice(&second.to_be_bytes());
            }
            for n in tail {
                out.extend_from_slice(&n.to_be_bytes());
            }
            out
        };

        // The curve: a count, then the pairs.
        let curve = pairs(&[3], &[(0.25, 0.31), (0.5, 0.28), (1.0, 0.26)], &[]);
        farm.generic_tick_tags.push((71, 546, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(71, 546, &curve)]), &mut context, &shared, &None,
        );
        assert_eq!(
            shared.market.paired_figures(instrument, 546),
            vec![(0.25, 0.31), (0.5, 0.28), (1.0, 0.26)],
            "the pairs, in the order the venue states them",
        );

        // The weights: a version, the count, the pairs, and a figure behind
        // them that is not a pair.
        let weights = pairs(&[1, 2], &[(330.0, 0.04), (340.0, 0.02)], &[7]);
        farm.generic_tick_tags.push((72, 490, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(72, 490, &weights)]), &mut context, &shared, &None,
        );
        assert_eq!(
            shared.market.paired_figures(instrument, 490),
            vec![(330.0, 0.04), (340.0, 0.02)],
            "the version in front of the count is not read as the count",
        );
        assert_eq!(
            shared.market.paired_figures_series(instrument), vec![490, 546],
            "both series are named as stated",
        );

        shared.market.forget_option_model(instrument);
        assert!(
            shared.market.paired_figures_series(instrument).is_empty(),
            "a figure outlived the contract it was stated for",
        );
    }

    /// The other three series that state two numbered tables, read by the
    /// same reader the extremes are.
    ///
    /// The whole table states four bytes to the figure as an integer and the
    /// fractional one four bytes as a fraction, so a reader holding both to
    /// one width reads one table's figures as the other's.
    #[test]
    fn the_other_series_that_number_their_figures_are_read_the_same_way() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(8003);

        let mut payload = 2i32.to_be_bytes().to_vec();
        for (named, value) in [(11i32, 4200i32), (12, -3)] {
            payload.extend_from_slice(&named.to_be_bytes());
            payload.extend_from_slice(&value.to_be_bytes());
        }
        payload.extend_from_slice(&1i32.to_be_bytes());
        payload.extend_from_slice(&30i32.to_be_bytes());
        payload.extend_from_slice(&225.5f32.to_be_bytes());

        farm.generic_tick_tags.push((61, 757, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(61, 757, &payload)]), &mut context, &shared, &None,
        );
        assert_eq!(
            shared.market.numbered_figures(instrument, 757, false),
            vec![(11, 4200.0), (12, -3.0)],
            "the whole table, under the venue's own numbering",
        );
        assert_eq!(
            shared.market.numbered_figures(instrument, 757, true), vec![(30, 225.5)],
            "and the fractional one, four bytes to the figure",
        );
        // These carry no documented call, so nothing goes out as a tick.
        assert!(
            shared.market.drain_series_ticks(instrument).is_empty(),
            "a figure with no documented call was sent under a number anyway",
        );

        // A slot handed back takes them with it.
        shared.market.forget_option_model(instrument);
        assert!(
            shared.market.numbered_figures_series(instrument).is_empty(),
            "a figure outlived the contract it was stated for",
        );
    }

    /// The series that state figures, read at the venue's own widths and in
    /// its own order.
    ///
    /// A reader taking every field as a double reads a single-precision field
    /// and the one behind it as one number, and reads an integer and its
    /// neighbour the same way: the widths are the record, not a detail of it.
    #[test]
    fn a_series_that_states_figures_is_read_at_the_widths_it_states_them_in() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(7002);

        let mut in_the_money = 1.5f64.to_be_bytes().to_vec();
        in_the_money.extend_from_slice(&1i32.to_be_bytes());
        in_the_money.extend_from_slice(&0.25f64.to_be_bytes());
        farm.generic_tick_tags.push((51, 493, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(51, 493, &in_the_money)]), &mut context, &shared, &None,
        );
        assert_eq!(
            shared.market.stated_figures(instrument, 493), vec![1.5, 1.0, 0.25],
            "the eight-byte figures and the four-byte one between them",
        );

        // A record of singles behind an integer: read as doubles, the first
        // figure swallows the two singles behind it.
        let mut theoretical = 3i32.to_be_bytes().to_vec();
        theoretical.extend_from_slice(&0.5f32.to_be_bytes());
        theoretical.extend_from_slice(&12.25f32.to_be_bytes());
        farm.generic_tick_tags.push((52, 613, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(52, 613, &theoretical)]), &mut context, &shared, &None,
        );
        assert_eq!(
            shared.market.stated_figures(instrument, 613), vec![3.0, 0.5, 12.25],
            "the single-precision figures are read four bytes wide",
        );

        // A record that stops before its trailing fields states the ones it
        // carried, and the series that never carries them is read whole.
        let mut schedule = Vec::new();
        for value in [10.0f64, 20.0, 30.0] {
            schedule.extend_from_slice(&value.to_be_bytes());
        }
        schedule.extend_from_slice(&7i32.to_be_bytes());
        farm.generic_tick_tags.push((53, 402, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(53, 402, &schedule)]), &mut context, &shared, &None,
        );
        assert_eq!(
            shared.market.stated_figures(instrument, 402), vec![10.0, 20.0, 30.0, 7.0],
            "the fields the venue did not write are not read as nothing",
        );

        assert_eq!(
            shared.market.stated_figures_series(instrument), vec![402, 493, 613],
            "every series that stated figures is named",
        );

        // A slot handed back takes its figures with it: the next contract in
        // it was never the one these were stated for.
        shared.market.forget_option_model(instrument);
        assert!(
            shared.market.stated_figures_series(instrument).is_empty(),
            "a figure outlived the contract it was stated for",
        );
    }

    /// The other thirteen series that carry the venue's own fields as text,
    /// each out from behind what it states in front of it.
    ///
    /// Two of them step over four bytes the way the insider record does. The
    /// rest state the text's length first and pad what follows to a four-byte
    /// boundary, so a reader taking the whole remainder takes padding with it,
    /// and one taking a stated length on trust takes bytes that never arrived.
    #[test]
    fn a_series_that_frames_its_text_is_read_out_from_behind_the_frame() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(4001);

        // A length, the text, and padding to the next four-byte boundary.
        let framed = |text: &[u8]| {
            let mut out = (text.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(text);
            out.resize(out.len().next_multiple_of(4), 0);
            out
        };

        farm.generic_tick_tags.push((41, 703, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(41, 703, &framed(b"LongInitial=25;MarginUnit=PCT"))]),
            &mut context, &shared, &None,
        );
        assert_eq!(
            shared.reference.company_data(4001, 703),
            vec![("LongInitial".to_string(), "25".to_string()),
                 ("MarginUnit".to_string(), "PCT".to_string())],
            "the padding behind the text is not read as part of a value",
        );

        // Four bytes of its own, then the text, the way the insider record
        // states it.
        let mut ratios = vec![0u8, 0, 0, 2];
        ratios.extend_from_slice(b"TTMREV=1.5;TTMEPS=0.25");
        farm.generic_tick_tags.push((42, 669, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(42, 669, &ratios)]), &mut context, &shared, &None,
        );
        assert_eq!(
            shared.reference.company_data(4001, 669),
            vec![("TTMREV".to_string(), "1.5".to_string()),
                 ("TTMEPS".to_string(), "0.25".to_string())],
            "the four bytes before the text are not read as part of a key",
        );

        // An alphabet of one byte to the character. The same bytes read as the
        // alphabet this client reads other text in are not text at all, and a
        // reader holding every series to that would publish nothing here.
        farm.generic_tick_tags.push((43, 726, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(43, 726, &framed(b"NAME=Nestl\xe9"))]),
            &mut context, &shared, &None,
        );
        assert_eq!(
            shared.reference.company_data(4001, 726),
            vec![("NAME".to_string(), "Nestlé".to_string())],
            "a byte that stands for a character in the venue's alphabet was dropped",
        );

        // A length reaching past what arrived states text this record does not
        // carry, and a length of nothing states none.
        for (slot, stated) in [(44u32, 64u32), (45, 0)] {
            let mut short = stated.to_be_bytes().to_vec();
            short.extend_from_slice(b"PASS=1");
            farm.generic_tick_tags.push((slot, 752, instrument));
            farm.handle_generic_tick(
                &framed_generic_ticks(&[(slot, 752, &short)]), &mut context, &shared, &None,
            );
            assert!(
                shared.reference.company_data(4001, 752).is_empty(),
                "a length of {stated} against six bytes of text published something anyway",
            );
        }
    }

    /// A record stating more estimate points than the series carries is not
    /// that record, and publishes nothing.
    ///
    /// The venue states three points and a word saying which of them stand.
    /// Read as three of however many it stated, the word was read out of the
    /// middle of a fourth price — and a fourth price whose low bits happen to
    /// say so published an estimate the venue never marked as standing.
    #[test]
    fn an_estimate_stating_more_points_than_it_carries_publishes_nothing() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);

        // Four prices where the venue states three, and the fourth begins with
        // the bytes that read as "the estimate stands". Read as three of the
        // four, the word saying which prices stand is the head of that fourth
        // price.
        let mut payload = Vec::new();
        payload.extend_from_slice(&0i32.to_be_bytes());
        payload.extend_from_slice(&4i32.to_be_bytes());
        for price in [31.0f64, 32.0, 33.0] {
            payload.extend_from_slice(&price.to_be_bytes());
        }
        payload.extend_from_slice(&[0, 0, 0, 3, 0, 0, 0, 0]);
        payload.extend_from_slice(&0i32.to_be_bytes());
        farm.generic_tick_tags.push((41, 586, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(41, 586, &payload)]), &mut context, &shared, &None,
        );
        assert!(
            shared.market.drain_series_ticks(instrument).is_empty(),
            "a record stating four points published one of them",
        );

        // And the record the venue does state still reads: three points and
        // the word behind them.
        let mut payload = Vec::new();
        payload.extend_from_slice(&0i32.to_be_bytes());
        payload.extend_from_slice(&3i32.to_be_bytes());
        for price in [31.0f64, 32.0, 33.0] {
            payload.extend_from_slice(&price.to_be_bytes());
        }
        payload.extend_from_slice(&3i32.to_be_bytes());
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(41, 586, &payload)]), &mut context, &shared, &None,
        );
        let said: Vec<(i32, String)> = shared.market.drain_series_ticks(instrument)
            .into_iter()
            .map(|t| (t.tick_type, format!("{:?}", t.value)))
            .collect();
        assert_eq!(said.len(), 1, "the middle of the estimate, and nothing else: {said:?}");
        assert_eq!(said[0].0, 101);
    }

    /// A series the venue has nothing to say on reaches nobody.
    ///
    /// It says so by stating the largest figure the field holds — the largest
    /// double where the record carries a double, the largest single where it
    /// carries one of those, the largest signed integer where it counts. The
    /// reference client publishes none of those, and a caller handed one reads
    /// two hundred undecillion dollars of borrow cost or two billion contracts
    /// of open interest as a reading.
    #[test]
    fn a_series_the_venue_has_nothing_to_say_on_reaches_nobody() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);

        // Option volume: two counts, neither held.
        let mut counted = Vec::new();
        counted.extend_from_slice(&i32::MAX.to_be_bytes());
        counted.extend_from_slice(&i32::MAX.to_be_bytes());
        farm.generic_tick_tags.push((21, 100, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(21, 100, &counted)]), &mut context, &shared, &None,
        );

        // The borrow cost and the regular session's last trade, both stated
        // as doubles.
        farm.generic_tick_tags.push((22, 499, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(22, 499, &f64::MAX.to_be_bytes())]),
            &mut context, &shared, &None,
        );
        farm.generic_tick_tags.push((23, 318, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(23, 318, &f64::MAX.to_be_bytes())]),
            &mut context, &shared, &None,
        );

        // And the auction, whose price is a single and whose two counts are
        // integers.
        let mut auction = Vec::new();
        auction.extend_from_slice(&i32::MAX.to_be_bytes());
        auction.extend_from_slice(&i32::MAX.to_be_bytes());
        auction.extend_from_slice(&f32::MAX.to_be_bytes());
        farm.generic_tick_tags.push((24, 225, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(24, 225, &auction)]), &mut context, &shared, &None,
        );

        let said = shared.market.drain_series_ticks(instrument);
        assert!(
            said.is_empty(),
            "the venue said nothing and the caller was told something: {said:?}",
        );
    }

    /// What an extra series states reaches the caller, under the number the
    /// reference client publishes it under.
    ///
    /// Asking for a series and reading it are separate things, and for a long
    /// time this client did only the first: the venue served what was asked
    /// for and the payloads were stepped over, so a caller who asked for the
    /// shortable count or the trade rate waited on a stream that was arriving
    /// and reaching nobody.
    #[test]
    fn what_an_extra_series_states_reaches_the_caller() {
        use crate::types::SeriesValue;
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);

        // Shortability: the flag, then the borrowable count behind it.
        let mut payload = Vec::new();
        payload.extend_from_slice(&3i32.to_be_bytes());
        payload.extend_from_slice(&40_000i32.to_be_bytes());
        farm.generic_tick_tags.push((11, 236, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(11, 236, &payload)]), &mut context, &shared, &None,
        );

        // The rate of volume, which the venue states as one number.
        farm.generic_tick_tags.push((12, 295, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(12, 295, &1234.5f64.to_be_bytes())]),
            &mut context, &shared, &None,
        );

        let said: Vec<(i32, String)> = shared.market.drain_series_ticks(instrument)
            .into_iter()
            .map(|t| (t.tick_type, match t.value {
                SeriesValue::Generic(v) => format!("generic {v}"),
                SeriesValue::Size(v) => format!("size {v}"),
                SeriesValue::Price(v) => format!("price {v}"),
                SeriesValue::Text(v) => format!("text {v}"),
            }))
            .collect();
        assert_eq!(
            said,
            [
                (46, "generic 3".to_string()),
                (89, "size 40000".to_string()),
                (56, "generic 1234.5".to_string()),
            ],
            "each reading under the number a caller reads it by",
        );

        // The auction, which states three things at once, and a future's open
        // interest, which the venue leaves unstated rather than stating none.
        let mut auction = Vec::new();
        auction.extend_from_slice(&5_000i32.to_be_bytes());
        auction.extend_from_slice(&(-250i32).to_be_bytes());
        auction.extend_from_slice(&101.25f32.to_be_bytes());
        // And, past the auction's own type and six figures the venue keeps for
        // itself, the imbalance it must publish.
        auction.extend_from_slice(&(b'O' as i32).to_be_bytes());
        for _ in 0..7 {
            auction.extend_from_slice(&i32::MAX.to_be_bytes());
        }
        auction.extend_from_slice(&(-1_200i32).to_be_bytes());
        farm.generic_tick_tags.push((13, 225, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(13, 225, &auction)]), &mut context, &shared, &None,
        );
        farm.generic_tick_tags.push((14, 588, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(14, 588, &i32::MAX.to_be_bytes())]),
            &mut context, &shared, &None,
        );
        let said: Vec<(i32, String)> = shared.market.drain_series_ticks(instrument)
            .into_iter()
            .map(|t| (t.tick_type, match t.value {
                SeriesValue::Generic(v) => format!("generic {v}"),
                SeriesValue::Size(v) => format!("size {v}"),
                SeriesValue::Price(v) => format!("price {v}"),
                SeriesValue::Text(v) => format!("text {v}"),
            }))
            .collect();
        assert_eq!(
            said,
            [
                (34, "size 5000".to_string()),
                (36, "size -250".to_string()),
                (35, "price 101.25".to_string()),
                (61, "size -1200".to_string()),
            ],
            "the auction states four things; an unstated open interest states none",
        );

        // The average option volume is the two sides added, and unstated
        // altogether when either side is.
        let mut avg = Vec::new();
        avg.extend_from_slice(&300i32.to_be_bytes());
        avg.extend_from_slice(&700i32.to_be_bytes());
        farm.generic_tick_tags.push((15, 105, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(15, 105, &avg)]), &mut context, &shared, &None,
        );
        let mut half = Vec::new();
        half.extend_from_slice(&300i32.to_be_bytes());
        half.extend_from_slice(&i32::MAX.to_be_bytes());
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(15, 105, &half)]), &mut context, &shared, &None,
        );
        let said: Vec<(i32, String)> = shared.market.drain_series_ticks(instrument)
            .into_iter()
            .map(|t| (t.tick_type, match t.value {
                SeriesValue::Generic(v) => format!("generic {v}"),
                SeriesValue::Size(v) => format!("size {v}"),
                SeriesValue::Price(v) => format!("price {v}"),
                SeriesValue::Text(v) => format!("text {v}"),
            }))
            .collect();
        assert_eq!(
            said, [(87, "size 1000".to_string())],
            "the two sides added, and nothing at all where one is unstated",
        );

        // The company ratios arrive as compressed text behind a header the
        // venue does not describe.
        use std::io::Write as _;
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(b"  MKTCAP=1234;PEEXCLXOR=18.2  ").unwrap();
        let mut ratios = vec![0u8; 8];
        ratios.extend_from_slice(&z.finish().unwrap());
        farm.generic_tick_tags.push((16, 258, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(16, 258, &ratios)]), &mut context, &shared, &None,
        );
        let said = shared.market.drain_series_ticks(instrument);
        assert_eq!(said.len(), 1, "the ratios reach the caller: {said:?}");
        assert_eq!(said[0].tick_type, 47);
        let SeriesValue::Text(text) = &said[0].value else { panic!("stated as text") };
        assert_eq!(text, "MKTCAP=1234;PEEXCLXOR=18.2", "inflated and trimmed");
    }

    /// The series a caller can ask for beyond a quote, each read the way the
    /// venue writes it.
    ///
    /// Every one of these arrived and was stepped over: the request went out,
    /// the venue served it, and the payload was logged as something nothing
    /// here reads. A caller asking for the year's extremes, the mark, the
    /// dividend, a fund's value or the last few minutes' volume waited on a
    /// stream that was already arriving.
    #[test]
    fn the_series_beyond_a_quote_are_read_the_way_the_venue_writes_them() {
        use crate::types::SeriesValue;
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        let said = |shared: &SharedState| -> Vec<(i32, String)> {
            shared.market.drain_series_ticks(instrument)
                .into_iter()
                .map(|t| (t.tick_type, match t.value {
                    SeriesValue::Generic(v) => format!("generic {v}"),
                    SeriesValue::Size(v) => format!("size {v}"),
                    SeriesValue::Price(v) => format!("price {v}"),
                    SeriesValue::Text(v) => format!("text {v}"),
                }))
                .collect()
        };
        let serve = |farm: &mut FarmState, req: u32, code: u32, payload: &[u8],
                         context: &mut Context| {
            farm.generic_tick_tags.push((req, code, instrument));
            farm.handle_generic_tick(
                &framed_generic_ticks(&[(req, code, payload)]), context, &shared, &None,
            );
        };

        // The historical volatility, under each of the two numbers the series
        // answers to. A caller who named the second was acknowledged and then
        // handed nothing, for the life of the subscription.
        serve(&mut farm, 28, 104, &0.1725f64.to_be_bytes(), &mut context);
        assert_eq!(said(&shared), [(23, "generic 0.1725".to_string())]);
        serve(&mut farm, 29, 512, &0.1725f64.to_be_bytes(), &mut context);
        assert_eq!(
            said(&shared), [(23, "generic 0.1725".to_string())],
            "the same series, on the number a caller is likelier to have named",
        );

        // The premium of an index over the future written on it.
        serve(&mut farm, 30, 162, &2.75f64.to_be_bytes(), &mut context);
        assert_eq!(said(&shared), [(31, "generic 2.75".to_string())]);

        // Stated as the largest a double carries, it is not a figure at all.
        serve(&mut farm, 31, 162, &f64::MAX.to_be_bytes(), &mut context);
        assert!(said(&shared).is_empty(), "an unstated premium states nothing");

        // The extremes and the ordinary day's volume: a table of whole
        // numbers, then a table of fractional ones, each naming its entries.
        let mut stats = Vec::new();
        stats.extend_from_slice(&1i32.to_be_bytes());
        stats.extend_from_slice(&768i32.to_be_bytes());
        stats.extend_from_slice(&12_500i32.to_be_bytes());
        stats.extend_from_slice(&7i32.to_be_bytes());
        for (named, value) in [
            (201i32, 61.5f32), (202, 40.25), (203, 63.0), (204, 38.5),
            (205, 70.75), (206, 31.0),
            // What it opened at a year ago, which reaches no caller.
            (210, 44.0),
        ] {
            stats.extend_from_slice(&named.to_be_bytes());
            stats.extend_from_slice(&value.to_be_bytes());
        }
        serve(&mut farm, 32, 165, &stats, &mut context);
        assert_eq!(
            said(&shared),
            [
                (21, "size 12500".to_string()),
                (16, "price 61.5".to_string()), (15, "price 40.25".to_string()),
                (18, "price 63".to_string()), (17, "price 38.5".to_string()),
                (20, "price 70.75".to_string()), (19, "price 31".to_string()),
            ],
            "each extreme under its own number, and nothing for the rest",
        );

        // The mark, with the flags that say whether it stands. The venue
        // states it this way under one of its two numbers; under the other it
        // is a record of the venue's own fields, read where those are.
        let mark = |price: f64, flags: i32| {
            let mut p = price.to_be_bytes().to_vec();
            p.extend_from_slice(&flags.to_be_bytes());
            p
        };
        serve(&mut farm, 33, 232, &mark(101.5, 1), &mut context);
        assert_eq!(said(&shared), [(37, "price 101.5".to_string())]);
        serve(&mut farm, 34, 232, &mark(101.5, 0), &mut context);
        assert!(said(&shared).is_empty(), "the lowest bit unset is no mark");
        serve(&mut farm, 35, 232, &mark(101.5, 1 | 0x0800_0000), &mut context);
        assert!(said(&shared).is_empty(), "and the high bit overrides it");
        serve(&mut farm, 36, 232, &mark(-1.0, 1), &mut context);
        assert!(said(&shared).is_empty(), "minus one is no mark, not a mark of minus one");

        // What the contract pays out: four bytes of the venue's own, then one
        // line.
        let mut dividends = vec![0u8, 0, 0, 0];
        dividends.extend_from_slice(b"0.83,0.79,20260215,0.21
");
        serve(&mut farm, 37, 456, &dividends, &mut context);
        assert_eq!(said(&shared), [(59, "text 0.83,0.79,20260215,0.21".to_string())]);

        // A fund's value: last, frozen, and the day's two extremes.
        serve(&mut farm, 38, 577, &55.25f64.to_be_bytes(), &mut context);
        serve(&mut farm, 39, 623, &55.10f64.to_be_bytes(), &mut context);
        let mut band = 56.0f64.to_be_bytes().to_vec();
        band.extend_from_slice(&54.5f64.to_be_bytes());
        serve(&mut farm, 40, 614, &band, &mut context);
        assert_eq!(
            said(&shared),
            [
                (96, "price 55.25".to_string()),
                (97, "price 55.1".to_string()),
                (98, "price 56".to_string()), (99, "price 54.5".to_string()),
            ],
        );
        let mut backwards = 54.5f64.to_be_bytes().to_vec();
        backwards.extend_from_slice(&56.0f64.to_be_bytes());
        serve(&mut farm, 41, 614, &backwards, &mut context);
        assert!(said(&shared).is_empty(), "a high under its own low states neither");

        // A figure the venue does not hold is not a figure: the largest the
        // type carries is how it says so, and two billion shares is not a
        // day's volume.
        let mut unheld = Vec::new();
        unheld.extend_from_slice(&1i32.to_be_bytes());
        unheld.extend_from_slice(&768i32.to_be_bytes());
        unheld.extend_from_slice(&i32::MAX.to_be_bytes());
        unheld.extend_from_slice(&1i32.to_be_bytes());
        unheld.extend_from_slice(&201i32.to_be_bytes());
        unheld.extend_from_slice(&f32::MAX.to_be_bytes());
        serve(&mut farm, 44, 165, &unheld, &mut context);
        assert!(said(&shared).is_empty(), "neither of them is a reading");

        let mut unheld_span = 1i32.to_be_bytes().to_vec();
        unheld_span.extend_from_slice(&5i32.to_be_bytes());
        unheld_span.extend_from_slice(&i32::MAX.to_be_bytes());
        serve(&mut farm, 45, 595, &unheld_span, &mut context);
        assert!(said(&shared).is_empty(), "nor is a span it holds nothing for");

        serve(&mut farm, 46, 232, &mark(f64::MAX, 1), &mut context);
        assert!(said(&shared).is_empty(), "nor is a mark it does not hold");

        // A dividend line with nothing on it is not a reading.
        serve(&mut farm, 43, 456, &[0u8, 0, 0, 0, b'\n'], &mut context);
        assert!(said(&shared).is_empty(), "an empty line says nothing");

        // The last few minutes' volume, each span named by its length.
        let mut spans = 3i32.to_be_bytes().to_vec();
        for (minutes, volume) in [(5i32, 220i32), (10, 480), (3, 90)] {
            spans.extend_from_slice(&minutes.to_be_bytes());
            spans.extend_from_slice(&volume.to_be_bytes());
        }
        serve(&mut farm, 42, 595, &spans, &mut context);
        assert_eq!(
            said(&shared),
            [
                (64, "size 220".to_string()),
                (65, "size 480".to_string()),
                (63, "size 90".to_string()),
            ],
            "read by the span the venue names, not by where it sits",
        );
    }

    /// The odd lot reaches the caller: both prices, both sizes, both venues.
    ///
    /// It arrives on a record that says where it ends, which this client
    /// abandoned rather than read, so a caller who asked what nobody has to
    /// deal in round lots at was told nothing at all.
    #[test]
    fn the_odd_lot_reaches_the_caller() {
        use crate::types::SeriesValue;
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        context.market.set_min_tick(instrument, 0.01);
        context.market.set_size_tick(instrument, 1.0);
        shared.reference.set_smart_components(vec![
            crate::types::SmartComponent { bit_number: 2, exchange: "NYSE".into(), exchange_letter: "N".into() },
            crate::types::SmartComponent { bit_number: 5, exchange: "ARCA".into(), exchange_letter: "P".into() },
        ]);

        let mut bits: Vec<u8> = Vec::new();
        let push = |value: u64, width: usize, bits: &mut Vec<u8>| {
            for i in (0..width).rev() {
                bits.push(((value >> i) & 1) as u8);
            }
        };
        // The two prices, their sizes, and where each is quoted.
        let record = [
            (0u64, 10_125i64), (1, 10_150), (4, 30), (5, 70),
            (16, 0b100), (17, 0b100_000),
        ];
        for (n, (id, value)) in record.iter().enumerate() {
            push(*id, 5, &mut bits);
            push(u64::from(n + 1 < record.len()), 1, &mut bits);
            push(3, 2, &mut bits);
            push(u64::from(*value < 0), 1, &mut bits);
            push(value.unsigned_abs(), 31, &mut bits);
        }
        let mut payload = vec![0u8; bits.len().div_ceil(8)];
        for (i, &b) in bits.iter().enumerate() {
            if b == 1 {
                payload[i >> 3] |= 1 << (7 - (i & 7));
            }
        }

        farm.generic_tick_tags.push((80, 787, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(80, 787, &payload)]), &mut context, &shared, &None,
        );
        let said: Vec<(i32, String)> = shared.market.drain_series_ticks(instrument)
            .into_iter()
            .map(|t| (t.tick_type, match t.value {
                SeriesValue::Generic(v) => format!("generic {v}"),
                SeriesValue::Size(v) => format!("size {v}"),
                SeriesValue::Price(v) => format!("price {v}"),
                SeriesValue::Text(v) => format!("text {v}"),
            }))
            .collect();
        assert_eq!(
            said,
            [
                (107, "size 30".to_string()),
                (105, "price 101.25".to_string()),
                (109, "text N".to_string()),
                (108, "size 70".to_string()),
                (106, "price 101.5".to_string()),
                (110, "text P".to_string()),
            ],
        );

        // And where the venue has stated no size increment — which it does by
        // stating none, and which is also every record arriving before the
        // acknowledgement carrying one — a size is counted in whole ones. Read
        // against an increment of nought, a size the venue stated reached the
        // caller as nought.
        let bare = context.market.register(265598);
        context.market.set_min_tick(bare, 0.01);
        farm.generic_tick_tags.push((81, 787, bare));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(81, 787, &payload)]), &mut context, &shared, &None,
        );
        let sizes: Vec<(i32, f64)> = shared.market.drain_series_ticks(bare)
            .into_iter()
            .filter_map(|t| match t.value {
                SeriesValue::Size(v) => Some((t.tick_type, v)),
                _ => None,
            })
            .collect();
        assert_eq!(
            sizes, [(107, 30.0), (108, 70.0)],
            "the sizes the venue stated, counted in whole ones",
        );
    }

    /// The mark the venue keeps for a contract reaches the caller.
    ///
    /// It arrives as a record of the venue's own fields — no length of its
    /// own, the record saying where it ends — and this client abandoned the
    /// message rather than read one, so the mark arrived and reached nobody.
    #[test]
    fn the_mark_the_venue_keeps_reaches_the_caller() {
        use crate::types::SeriesValue;
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        // A penny a tick. The raw count is in tenths of one.
        context.market.set_min_tick(instrument, 0.01);

        // One record: the price, then the word of flags that ends it.
        let record = |price: i64, flags: i64| {
            let mut bits: Vec<u8> = Vec::new();
            let mut push = |value: u64, width: usize| {
                for i in (0..width).rev() {
                    bits.push(((value >> i) & 1) as u8);
                }
            };
            for (id, more, value) in [(2u64, 1u64, price), (13, 0, flags)] {
                push(id, 5);
                push(more, 1);
                push(3, 2); // four bytes wide
                push(u64::from(value < 0), 1);
                push(value.unsigned_abs(), 31);
            }
            let mut bytes = vec![0u8; bits.len().div_ceil(8)];
            for (i, &b) in bits.iter().enumerate() {
                if b == 1 {
                    bytes[i >> 3] |= 1 << (7 - (i & 7));
                }
            }
            bytes
        };

        farm.generic_tick_tags.push((70, 220, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(70, 220, &record(101_250, 0))]),
            &mut context, &shared, &None,
        );
        let said = shared.market.drain_series_ticks(instrument);
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].tick_type, 78);
        assert!(
            matches!(said[0].value, SeriesValue::Price(p) if (p - 101.25).abs() < 1e-9),
            "a hundred and one and a quarter: {:?}", said[0].value,
        );

        // The venue saying the mark does not stand.
        farm.generic_tick_tags.push((71, 220, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(71, 220, &record(101_250, 16))]),
            &mut context, &shared, &None,
        );
        assert!(
            shared.market.drain_series_ticks(instrument).is_empty(),
            "a mark the venue says does not stand is not a mark",
        );

        // The slow one answers on its own number.
        farm.generic_tick_tags.push((72, 619, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(72, 619, &record(101_000, 0))]),
            &mut context, &shared, &None,
        );
        let said = shared.market.drain_series_ticks(instrument);
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].tick_type, 79);
        assert!(
            matches!(said[0].value, SeriesValue::Price(p) if (p - 101.0).abs() < 1e-9),
            "a hundred and one on the tenths the venue counts in: {:?}", said[0].value,
        );

        // And the number the mark is also asked for under, which states the
        // same record and reaches the caller on the number the plain one does.
        farm.generic_tick_tags.push((73, 221, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(73, 221, &record(101_250, 0))]),
            &mut context, &shared, &None,
        );
        let said = shared.market.drain_series_ticks(instrument);
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].tick_type, 37);
        assert!(
            matches!(said[0].value, SeriesValue::Price(p) if (p - 101.25).abs() < 1e-9),
            "the mark, on its other number: {:?}", said[0].value,
        );
    }

    /// A record that states its own length is not the last thing in its
    /// message.
    ///
    /// The venue writes the mark as a run of its own fields, four bytes of it
    /// at one moment and twelve at the next, and puts the venue list behind it
    /// in the same message. Handing the rest of the message over as the mark's
    /// payload and stopping there read one record and threw the other away.
    #[test]
    fn a_record_that_states_its_own_length_is_followed_by_the_next_one() {
        use crate::types::SeriesValue;
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        context.market.set_min_tick(instrument, 0.01);

        // The mark, as one field that says nothing follows it: five bits of
        // number, one that says no more, two of width and a sign, and then the
        // count. Four bytes, whatever comes after them in the message.
        let mark = |price: u64| {
            // Number two in the top five bits, nothing saying more follows,
            // three bytes of width, no sign, and the count in what is left.
            let word: u64 = (2 << 27) | (2 << 24) | price;
            (word as u32).to_be_bytes().to_vec()
        };

        farm.generic_tick_reqs.push((80, 233));
        farm.generic_tick_tags.push((80, 233, instrument));
        farm.generic_tick_tags.push((81, 221, instrument));

        // The mark first, and a second record behind it in the same message.
        let totals = |value: f64, shares: i64, count: i32| {
            let mut bytes = value.to_be_bytes().to_vec();
            bytes.extend_from_slice(&shares.to_be_bytes());
            bytes.extend_from_slice(&count.to_be_bytes());
            bytes
        };
        farm.handle_generic_tick(
            &framed_generic_ticks(&[
                (81, 221, &mark(101_250)),
                (80, 233, &totals(10_000.0, 100, 1)),
            ]),
            &mut context, &shared, &None,
        );
        let said = shared.market.drain_series_ticks(instrument);
        assert_eq!(said.len(), 1, "the mark: {said:?}");
        assert_eq!(said[0].tick_type, 37);
        assert!(
            matches!(said[0].value, SeriesValue::Price(p) if (p - 101.25).abs() < 1e-9),
            "{:?}", said[0].value,
        );

        // The record behind it sets the running series' baseline, which says
        // nothing of its own — so a second reading is what shows it arrived.
        farm.handle_generic_tick(
            &framed_generic_ticks(&[
                (81, 221, &mark(101_250)),
                (80, 233, &totals(11_010.0, 110, 2)),
            ]),
            &mut context, &shared, &None,
        );
        let said = shared.market.drain_series_ticks(instrument);
        assert!(
            said.iter().any(|t| t.tick_type == 48),
            "the record behind the mark was read too: {said:?}",
        );
    }

    /// The two running series keep their own baselines.
    ///
    /// Everything that traded is one series; what traded on a trade report is
    /// another. Sharing one baseline, each reading of one states its trade
    /// against the other's totals, which is a print nobody made.
    #[test]
    fn each_running_series_states_its_trade_against_its_own_totals() {
        use crate::types::SeriesValue;
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        farm.generic_tick_tags.push((50, 233, instrument));
        farm.generic_tick_tags.push((51, 375, instrument));
        let totals = |value: f64, shares: i64, trades: i32| {
            let mut p = value.to_be_bytes().to_vec();
            p.extend_from_slice(&shares.to_be_bytes());
            p.extend_from_slice(&trades.to_be_bytes());
            p
        };
        let serve = |farm: &mut FarmState, req: u32, code: u32, payload: &[u8],
                     context: &mut Context| {
            farm.handle_generic_tick(
                &framed_generic_ticks(&[(req, code, payload)]), context, &shared, &None,
            );
        };

        // A baseline each, which states nothing on its own.
        serve(&mut farm, 50, 233, &totals(10_000.0, 100, 1), &mut context);
        serve(&mut farm, 51, 375, &totals(4_000.0, 40, 1), &mut context);
        assert!(shared.market.drain_series_ticks(instrument).is_empty(), "a baseline is not a trade");

        // Then one trade on each, read against its own baseline.
        serve(&mut farm, 50, 233, &totals(11_010.0, 110, 2), &mut context);
        serve(&mut farm, 51, 375, &totals(4_505.0, 45, 2), &mut context);
        let said: Vec<(i32, String)> = shared.market.drain_series_ticks(instrument)
            .into_iter()
            .map(|t| (t.tick_type, match t.value {
                SeriesValue::Text(v) => v,
                other => format!("{other:?}"),
            }))
            .collect();
        assert_eq!(said.len(), 2, "{said:?}");
        assert_eq!(said[0].0, 48);
        assert_eq!(said[1].0, 77);
        assert!(
            said[0].1.starts_with("101;10.0000"),
            "ten shares at a hundred and one: {}", said[0].1,
        );
        assert!(
            said[1].1.starts_with("101;5.0000"),
            "five shares at a hundred and one, off its own totals: {}", said[1].1,
        );
    }

    /// The totals a running series is read against go with the subscription.
    ///
    /// Left behind, the first reading after a contract is watched again is
    /// measured from a total the venue stated in another subscription: a print
    /// for everything that traded in between, or for a negative number of
    /// shares where the venue has started its day over.
    #[test]
    fn the_running_totals_are_released_with_the_subscription() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        farm.generic_tick_tags.push((60, 233, instrument));
        let totals = |value: f64, shares: i64, trades: i32| {
            let mut p = value.to_be_bytes().to_vec();
            p.extend_from_slice(&shares.to_be_bytes());
            p.extend_from_slice(&trades.to_be_bytes());
            p
        };
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(60, 233, &totals(10_000.0, 100, 1))]),
            &mut context, &shared, &None,
        );
        let _ = shared.market.drain_series_ticks(instrument);

        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], u64::MAX, false, &mut None, &mut hb);
        // Watched again, the venue starts its totals over.
        farm.generic_tick_tags.push((61, 233, instrument));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(61, 233, &totals(500.0, 5, 1))]),
            &mut context, &shared, &None,
        );
        assert!(
            shared.market.drain_series_ticks(instrument).is_empty(),
            "the first reading of a new subscription is a baseline, not a trade",
        );
    }

    /// The running volume states a trade, not the totals it is read from.
    ///
    /// The venue states what has traded by value, by shares and by count since
    /// the day began; a caller is owed the trade between two of those
    /// statements. Read as the totals themselves, a caller subscribing at
    /// noon would have been handed the whole morning as one print.
    #[test]
    fn the_running_volume_states_the_trade_between_two_totals() {
        use crate::types::SeriesValue;
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        farm.generic_tick_tags.push((21, 233, instrument));

        let totals = |value: f64, shares: i64, trades: i32| {
            let mut p = Vec::new();
            p.extend_from_slice(&value.to_be_bytes());
            p.extend_from_slice(&shares.to_be_bytes());
            p.extend_from_slice(&trades.to_be_bytes());
            p
        };

        // The first statement is a baseline and nothing else: there is no
        // earlier one to take a difference from.
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(21, 233, &totals(1_000_000.0, 10_000, 40))]),
            &mut context, &shared, &None,
        );
        assert!(
            shared.market.drain_series_ticks(instrument).is_empty(),
            "the first totals are a baseline, not a trade",
        );

        // A hundred shares at 101 apiece, on one trade.
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(21, 233, &totals(1_010_100.0, 10_100, 41))]),
            &mut context, &shared, &None,
        );
        let said = shared.market.drain_series_ticks(instrument);
        assert_eq!(said.len(), 1, "one statement for one trade");
        assert_eq!(said[0].tick_type, 48);
        let SeriesValue::Text(text) = &said[0].value else {
            panic!("the running volume is stated as text: {:?}", said[0].value)
        };
        let parts: Vec<&str> = text.split(';').collect();
        assert_eq!(parts.len(), 6, "six fields: {text}");
        assert_eq!(parts[0], "101", "what it traded at: {text}");
        assert_eq!(parts[1], "100.0000000000000000", "and how many: {text}");
        assert_eq!(parts[3], "10100.0000000000000000", "the day's shares so far: {text}");
        assert_eq!(
            parts[4], "100.00990099",
            "the average struck over the day, to the eight places the venue starts at: {text}",
        );
        assert_eq!(parts[5], "true", "one trade is a single trade: {text}");
    }

    /// A frame under a number nothing asked a generic tick under says nothing
    /// about which tick it is, so it is dropped rather than guessed at.
    /// Instrument 0 is a real instrument — the first one registered — so a
    /// guess would pin somebody else's article on it.
    #[test]
    fn a_tick_under_an_unasked_number_is_dropped_not_misattributed() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let first = context.market.register(756733);
        assert_eq!(first, 0, "the first instrument really is id 0");
        context.market.register_server_tag(999_999, first);

        let msg = framed_news(999_999, &one_article());
        farm.handle_generic_tick(&msg, &mut context, &shared, &None);
        assert!(
            shared.market.drain_tick_news().is_empty(),
            "a number nothing asked a generic tick under delivers nothing",
        );

        // Positive control: the same frame, once this client has said what it
        // asked for under that number.
        farm.generic_tick_tags.push((999_999, NEWS_REQUEST_TYPE, first));
        farm.handle_generic_tick(&msg, &mut context, &shared, &None);
        assert_eq!(
            shared.market.drain_tick_news().len(), 1,
            "so the drop above is what was asked for, not the frame",
        );
    }

    /// Which tick a frame carries is what was asked for under its number, not
    /// how long the frame is. Read off the length, every tick whose payload
    /// happened to be the size of an option model read as an option model.
    #[test]
    fn two_ticks_of_one_length_are_told_apart() {
        let shared = SharedState::new();
        let article = one_article();

        let mut context = Context::new();
        let mut as_news = FarmState::new();
        as_news.generic_tick_tags.push((7, NEWS_REQUEST_TYPE, 0));
        as_news.handle_generic_tick(&framed_news(7, &article), &mut context, &shared, &None);
        assert_eq!(shared.market.drain_tick_news().len(), 1);

        // The same bytes, the same length, asked for as something else.
        let mut as_status = FarmState::new();
        as_status.generic_tick_tags.push((7, TRADING_STATUS_REQUEST_TYPE, 0));
        as_status.handle_generic_tick(&framed_news(7, &article), &mut context, &shared, &None);
        assert!(
            shared.market.drain_tick_news().is_empty(),
            "the same bytes under a different tick are not an article",
        );
    }

    /// The venue says whether it has stopped a contract, why, and whether it is
    /// restricting short sales in it. All three reach a caller.
    ///
    /// Folded to a yes or no, the reason was unreachable — the field could only
    /// ever hold 0 or 1 where its own contract names three values — and the
    /// restriction reached nobody at all. The two reasons call for opposite
    /// handling: a volatility pause lifts on a clock and a regulator's halt
    /// does not, and a program routing a short into a restricted contract had
    /// the order bounced rather than knowing not to send it.
    #[test]
    fn a_halt_states_its_reason_and_a_short_sale_restriction_reaches_a_caller() {
        use crate::protocol::trading_status::TradingStatus;

        // A status record is three big-endian words: the mask, a stamp, and
        // the one status the venue names beside it.
        let record = |mask: u32, named: u32| {
            let mut body = Vec::new();
            body.extend_from_slice(&mask.to_be_bytes());
            body.extend_from_slice(&0u32.to_be_bytes());
            body.extend_from_slice(&named.to_be_bytes());
            body
        };
        let read = |mask: u32, named: u32| {
            let mut farm = FarmState::new();
            let mut context = Context::new();
            let shared = SharedState::new();
            let id = context.market.register(756733);
            context.market.register_server_tag(7, id);
            farm.generic_tick_tags.push((7, TRADING_STATUS_REQUEST_TYPE, 0));
            farm.handle_generic_tick(
                &framed_generic_ticks(&[(7, TRADING_STATUS_REQUEST_TYPE, &record(mask, named))]),
                &mut context, &shared, &None,
            );
            (context.quote(id).halted, shared.market.short_sale_restricted(id))
        };

        assert_eq!(
            read(TradingStatus::ExchangeOpen.mask(), TradingStatus::ExchangeOpen.index()),
            (0, false),
            "an open contract is not halted and not restricted",
        );
        assert_eq!(
            read(TradingStatus::RegulatoryHalt.mask(), TradingStatus::RegulatoryHalt.index()),
            (1, false),
            "a regulator's halt",
        );
        assert_eq!(
            read(TradingStatus::VolatilityHalt.mask(), TradingStatus::VolatilityHalt.index()),
            (2, false),
            "a volatility pause, which is the value a yes-or-no field could never reach",
        );
        assert_eq!(
            read(TradingStatus::ShortSaleRestriction.mask(), TradingStatus::None.index()),
            (0, true),
            "a restriction is not a halt, and the contract still trades",
        );
        // Read off the mask, not off the name beside it: the venue can stop a
        // contract and name nothing.
        assert_eq!(
            read(TradingStatus::VolatilityHalt.mask(), TradingStatus::None.index()),
            (2, false),
            "a halt the venue did not name is still a halt, and still has a reason",
        );
        // And both at once, which the venue does state: a regulator's halt
        // outranks, because it is the one that does not lift on a clock.
        assert_eq!(
            read(
                TradingStatus::RegulatoryHalt.mask()
                    | TradingStatus::VolatilityHalt.mask()
                    | TradingStatus::ShortSaleRestriction.mask(),
                TradingStatus::None.index(),
            ),
            (1, true),
            "more than one status is in force at once",
        );
    }

    /// The tick-165 payload a live AAPL subscription was sent, byte for byte.
    const A_CAPTURED_165_PAYLOAD: &str = "\
        00000001000003000338e7e60000000a000000c943ac48f6000000ca4388e000\
        000000cb43ac48f6000000cc43754831000000cd43ac48f6000000ce436a5be7\
        000000d043a81c29000000d143977831000000d2436492b000000198466408cd";

    /// The tick that carries a contract's price extremes carries two figures
    /// about the company as well, and both reached nobody.
    ///
    /// The payload is the one a live AAPL subscription was sent, byte for byte:
    /// one whole-number entry and ten fractional ones. How many shares are on
    /// issue is the multiplier that turns a price into a market
    /// capitalisation, and the documented API reaches it only through a
    /// fundamentals request of its own; what the contract opened at a year ago
    /// has no call there at all. Both were read past to step the cursor.
    ///
    /// The share count is stated in millions. Settled on two live contracts
    /// three orders of magnitude apart — this one, whose company has about
    /// fourteen and a half billion shares, and one with about four billion —
    /// and nothing else in the table is within three orders of magnitude of
    /// either, which is what makes it a reading rather than a guess.
    #[test]
    fn the_tick_that_carries_the_extremes_carries_the_company_too() {
        // 96 bytes, as the venue sent them.
        let payload: Vec<u8> = (0..)
            .step_by(2)
            .take(96)
            .map(|i| u8::from_str_radix(&A_CAPTURED_165_PAYLOAD[i..i + 2], 16).unwrap())
            .collect();

        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let id = context.market.register(265598);
        context.market.register_server_tag(7, id);
        farm.generic_tick_tags.push((7, 165, 0));
        farm.handle_generic_tick(
            &framed_generic_ticks(&[(7, 165, &payload)]), &mut context, &shared, &None,
        );

        let figures = shared.market.contract_figures(id).expect("the venue stated them");
        assert!(
            (figures.shares_outstanding - 14_594_200_195.312_5).abs() < 1.0,
            "fourteen and a half billion shares, not fourteen thousand: {figures:?}",
        );
        assert!(
            (figures.open_a_year_ago - 228.572_998_046_875).abs() < 1e-9,
            "{figures:?}",
        );

        // The extremes beside them still go out as the ticks they always did.
        let ticks = shared.market.drain_series_ticks(id);
        let at = |tick: i32| ticks.iter().find(|t| t.tick_type == tick).map(|t| match t.value {
            crate::types::SeriesValue::Price(v)
            | crate::types::SeriesValue::Size(v)
            | crate::types::SeriesValue::Generic(v) => v,
            crate::types::SeriesValue::Text(_) => f64::NAN,
        });
        assert_eq!(at(16), Some(344.570_007_324_218_75), "the thirteen-week high");
        assert_eq!(at(19), Some(234.358_993_530_273_44), "and the fifty-two week low");

        // And every figure in the table is kept under the venue's own number,
        // including the ones with no documented call to arrive on. Read for
        // the two that have one and dropped otherwise, a figure the venue
        // started stating would go unseen until someone here wrote its number
        // down.
        let fractional = shared.market.numbered_figures(id, 165, true);
        for kind in [208, 209] {
            assert!(
                fractional.iter().any(|(named, _)| *named == kind),
                "a figure of kind {kind} was not kept: {fractional:?}",
            );
        }
        assert_eq!(
            fractional.iter().find(|(named, _)| *named == 201).map(|(_, v)| *v),
            Some(344.570_007_324_218_75),
            "the figure a documented call carries is kept beside the rest",
        );
        assert_eq!(
            shared.market.numbered_figures_series(id), vec![165],
            "the series that stated them is named",
        );
    }

    /// A message carries one record after another, and each is delivered. Read
    /// as a single record, everything after the first went unread.
    #[test]
    fn every_record_in_a_message_is_read() {
        let article = one_article();
        let msg = framed_generic_ticks(&[
            (7, NEWS_REQUEST_TYPE, &article),
            (9, NEWS_REQUEST_TYPE, &article),
        ]);

        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        farm.generic_tick_tags.push((7, NEWS_REQUEST_TYPE, 0));
        farm.generic_tick_tags.push((9, NEWS_REQUEST_TYPE, 1));
        farm.handle_generic_tick(&msg, &mut context, &shared, &None);

        let delivered = shared.market.drain_tick_news();
        assert_eq!(delivered.len(), 2, "the second record went unread");
        assert_eq!(delivered[0].instrument, 0);
        assert_eq!(delivered[1].instrument, 1);
    }

    /// Where a record ends depends on the tick it carries, so a number nothing
    /// asked for stops the reading. Carrying on would read the next record
    /// from the middle of this one and deliver whatever that happened to spell.
    #[test]
    fn an_unasked_number_stops_the_reading() {
        let article = one_article();
        let msg = framed_generic_ticks(&[
            (5, NEWS_REQUEST_TYPE, &article),
            (7, NEWS_REQUEST_TYPE, &article),
        ]);

        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        // Only the second record's number is known.
        farm.generic_tick_tags.push((7, NEWS_REQUEST_TYPE, 0));
        farm.handle_generic_tick(&msg, &mut context, &shared, &None);
        assert!(
            shared.market.drain_tick_news().is_empty(),
            "reading carried on past a record whose end was unknown",
        );
    }

    /// The venue states the length in bits in two bytes, so it wraps at eight
    /// thousand one hundred and ninety-two. What was carried is recovered
    /// against how much arrived, or a long message is cut off in the middle
    /// with nothing to say it had been.
    #[test]
    fn a_message_longer_than_the_length_field_holds_is_recovered() {
        assert_eq!(generic_tick_length(168, 23), Some(21));
        // Nine thousand bytes: the stated count has wrapped once, and what
        // arrived is what says so.
        let carried = 9_000usize;
        let stated = ((carried * 8) % 65_536) as u16;
        assert_eq!(generic_tick_length(stated, carried + 2), Some(carried));
    }

    /// A caller joining a contract already being watched has the series it
    /// named asked for, and the ones already being served are not asked twice.
    ///
    /// The list that went to the venue is the first caller's. A joiner naming
    /// a series nobody had asked for waited on a stream that was never
    /// requested — the prices arrived, the subscription read as healthy, and
    /// the series never came.
    #[test]
    fn a_joining_caller_has_the_series_it_named_asked_for() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.asked_generic_ticks.insert(instrument, vec![233]);
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        let _ = super::drain_inner(&mut peer);

        // A second caller on the same contract, naming one series already
        // being served and one that is not.
        farm.also_ask_for_series(instrument, 756733, &[233, 236], &context, &mut conn, &mut hb);

        let stated = |msg: &[u8], tag: u32| -> Vec<String> {
            let prefix = format!("{tag}=");
            msg.split(|&b| b == 0x01)
                .filter_map(|field| {
                    std::str::from_utf8(field).ok()?.strip_prefix(prefix.as_str()).map(str::to_string)
                })
                .collect()
        };
        let asked: Vec<String> = super::drain_inner(&mut peer)
            .into_iter()
            .filter(|msg| stated(msg, 263).first().map(String::as_str) == Some("1"))
            .flat_map(|msg| stated(&msg, 264))
            .collect();
        assert_eq!(
            asked, ["236".to_string()],
            "the series nobody had asked for, and only that one: {asked:?}",
        );
        assert_eq!(
            farm.asked_generic_ticks.get(&instrument).map(Vec::as_slice),
            Some([233u32, 236].as_slice()),
            "and both are what the rebuild after a reconnect asks for",
        );
        // It is an entry of the subscription, so the withdrawal states it.
        let record = farm.instrument_md_reqs.iter()
            .find(|(id, _)| *id == instrument)
            .map(|(_, record)| record)
            .expect("the subscription is recorded");
        assert!(
            record.entries.iter().any(|e| e.request_type == 236),
            "the joiner's series is withdrawn with the rest: {:?}",
            record.entries.iter().map(|e| e.request_type).collect::<Vec<_>>(),
        );
    }

    /// A series named while the connection is down is what the rebuild asks
    /// for.
    ///
    /// The wire record goes with the connection and the caller's list stays,
    /// because the list is what the rebuild reads. Written only once a live
    /// record had been found, a series named in between was dropped: the call
    /// answered, the rebuild asked for the list as it stood, and the caller
    /// watched for a series nobody had asked the venue for.
    #[test]
    fn a_series_named_while_the_wire_is_down_is_what_the_rebuild_asks_for() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        let _ = super::drain_inner(&mut peer);

        // The connection goes away. What the callers asked for survives it;
        // the record of what is on the wire does not.
        farm.handle_disconnect(&mut conn, &mut context, &None, &crate::bridge::SharedState::new());

        // And a caller joins the client-side stream, naming a series of its
        // own.
        farm.also_ask_for_series(instrument, 756733, &[236], &context, &mut conn, &mut hb);

        assert_eq!(
            farm.asked_generic_ticks.get(&instrument).map(Vec::as_slice),
            Some([236u32].as_slice()),
            "the rebuild after the reconnect asks for it",
        );
    }

    /// A series nobody asks for any more is withdrawn as itself, and the rest
    /// of the subscription stands.
    ///
    /// The caller that brought a series withdraws while the subscription it
    /// joined stays up for whoever opened it. Left asked for, the venue served
    /// it for the life of that subscription with nobody reading it, and the
    /// rebuild after a reconnect asked for it again.
    #[test]
    fn a_series_nobody_asks_for_is_withdrawn_as_itself() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        farm.also_ask_for_series(instrument, 756733, &[236], &context, &mut conn, &mut hb);
        let _ = super::drain_inner(&mut peer);

        farm.stop_asking_for_series(instrument, 0, 0, &[236], u64::MAX, &mut conn, &mut hb);

        let stated = |msg: &[u8], tag: u32| -> Vec<String> {
            let prefix = format!("{tag}=");
            msg.split(|&b| b == 0x01)
                .filter_map(|field| {
                    std::str::from_utf8(field).ok()?.strip_prefix(prefix.as_str()).map(str::to_string)
                })
                .collect()
        };
        let withdrawn: Vec<String> = super::drain_inner(&mut peer)
            .into_iter()
            .filter(|msg| stated(msg, 263).first().map(String::as_str) == Some("2"))
            .flat_map(|msg| stated(&msg, 264))
            .collect();
        assert_eq!(
            withdrawn, ["236".to_string()],
            "the series it brought, and nothing else of the subscription: {withdrawn:?}",
        );
        assert!(
            !farm.asked_generic_ticks.get(&instrument)
                .is_some_and(|asked| asked.contains(&236)),
            "the rebuild after a reconnect does not ask for it again",
        );
        let record = farm.instrument_md_reqs.iter()
            .find(|(id, _)| *id == instrument)
            .map(|(_, record)| record)
            .expect("the subscription stands");
        assert!(
            !record.entries.iter().any(|e| e.request_type == 236),
            "and it is no longer one of its entries: {:?}",
            record.entries.iter().map(|e| e.request_type).collect::<Vec<_>>(),
        );
        assert!(
            record.entries.iter().any(|e| e.request_type != 236),
            "while the rest of the subscription is still being served",
        );
    }

    /// A caller that named what every subscription already asks for does not
    /// withdraw it.
    ///
    /// The trading status, the venue map and the option model are opened by
    /// the subscription itself, so a caller naming one of them is not asked
    /// for a second subscription to it — and it is not that caller's to
    /// withdraw either. Withdrawn with it, the subscription that goes on
    /// running lost its trading status, and nothing asked for it again until a
    /// reconnect.
    #[test]
    fn a_caller_does_not_withdraw_what_the_subscription_asks_for_itself() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        let _ = super::drain_inner(&mut peer);

        // A caller that named the trading status withdraws.
        farm.stop_asking_for_series(instrument, 0, 0, &[TRADING_STATUS_REQUEST_TYPE], u64::MAX, &mut conn, &mut hb,
        );

        let record = farm.instrument_md_reqs.iter()
            .find(|(id, _)| *id == instrument)
            .map(|(_, record)| record)
            .expect("the subscription stands");
        assert!(
            record.entries.iter().any(|e| e.request_type == TRADING_STATUS_REQUEST_TYPE),
            "the subscription still asks for the status it opens with: {:?}",
            record.entries.iter().map(|e| e.request_type).collect::<Vec<_>>(),
        );
        assert!(
            super::drain_inner(&mut peer).is_empty(),
            "and nothing was withdrawn on the wire",
        );
    }

    /// Giving up a series while the connection is down forgets when it was
    /// asked for.
    ///
    /// There is no wire state to take while the farm is down, so the withdrawal
    /// stops at the list the rebuild reads. The number beside each series is
    /// part of that list: left behind, a contract joined and given up often
    /// enough grew the record for the life of the session.
    #[test]
    fn giving_up_a_series_while_the_wire_is_down_forgets_when_it_was_asked_for() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let mut conn = None;

        // A subscription the reconnect will bring back, and a series named on
        // it while the connection is down.
        farm.md_resub_info.push((
            instrument, "SPY".into(), "SMART".into(), "STK".into(), String::new(),
            0.0, String::new(), String::new(), 0,
        ));
        farm.also_ask_for_series(instrument, 756733, &[236], &context, &mut conn, &mut hb);
        farm.note_series_asked_on(instrument, &[236], 4);

        farm.stop_asking_for_series(instrument, 0, 0, &[236], 5, &mut conn, &mut hb);

        assert!(
            !farm.asked_generic_ticks.get(&instrument).is_some_and(|a| a.contains(&236)),
            "the rebuild does not ask for it",
        );
        assert!(
            !farm.series_asked_on.contains_key(&(instrument, 236)),
            "and nothing is left behind saying when it was asked for",
        );
    }

    /// A withdrawal naming the slot a caller was moved off reaches what that
    /// caller asked for.
    ///
    /// A caller whose contract turns out to live in another slot is sent there,
    /// and what it asked for beyond the quote goes with it. Its withdrawal
    /// still names the slot it was given: applied there, it found nothing, and
    /// the venue went on serving that series where the caller had been moved to
    /// with nobody asking for it.
    #[test]
    fn a_withdrawal_naming_the_slot_a_caller_left_reaches_what_it_asked_for() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let into = context.market.register(756733);
        let from: InstrumentId = into + 1;
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        // The subscription the caller was sent to, with the series it brought.
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", into, 0,
            false, &mut conn, &mut hb,
        );
        farm.also_ask_for_series(into, 756733, &[236], &context, &mut conn, &mut hb);
        farm.note_series_asked_on(into, &[236], 4);
        farm.note_moved(from, into);
        let _ = super::drain_inner(&mut peer);

        // Its withdrawal names the slot it was given, not the one it was sent
        // to.
        farm.send_mktdata_unsubscribe(from, 0, 0, &[236], 5, false, &mut conn, &mut hb);

        assert!(
            !farm.asked_generic_ticks.get(&into).is_some_and(|a| a.contains(&236)),
            "the series it asked for is given up where it was being served",
        );
        assert!(
            farm.instrument_md_reqs.iter().any(|(id, _)| *id == into),
            "and the subscription the other callers are on stands",
        );
    }

    /// A withdrawal names the contract it was about, not only the slot.
    ///
    /// A slot goes to the next contract that needs one, so its number says
    /// nothing about which occupancy ended. A withdrawal decided against the
    /// contract that left took down the subscription of the one that arrived:
    /// that caller was published as watching the contract, and heard nothing
    /// for the rest of the session.
    #[test]
    fn a_withdrawal_names_the_contract_it_was_about() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        // The subscription on the slot went out for one contract.
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        let _ = super::drain_inner(&mut peer);

        // A withdrawal decided against the contract that held the slot before
        // it, with a number later than anything this subscription was asked
        // under.
        farm.send_mktdata_unsubscribe(instrument, 265_598, 0, &[], u64::MAX, false, &mut conn, &mut hb);
        assert!(
            farm.instrument_md_reqs.iter().any(|(id, _)| *id == instrument),
            "the subscription for the contract now on the slot stands",
        );
        assert!(
            super::drain_inner(&mut peer).is_empty(),
            "and nothing was withdrawn on the wire",
        );

        // The caller that named this contract withdraws it.
        farm.send_mktdata_unsubscribe(instrument, 756_733, 0, &[], u64::MAX, false, &mut conn, &mut hb);
        assert!(
            !farm.instrument_md_reqs.iter().any(|(id, _)| *id == instrument),
            "the contract's own caller can withdraw it",
        );
    }

    /// A withdrawal follows every move its callers were sent through.
    ///
    /// A contract can be moved more than once: the slot a caller was sent to
    /// can itself turn out to hold a contract that lives somewhere else again.
    /// Followed one hop, a withdrawal decided against the first slot reached
    /// the second and did nothing there, and the venue went on serving the
    /// series where the caller had finally been sent.
    #[test]
    fn a_withdrawal_follows_every_move_its_callers_were_sent_through() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let last = context.market.register(756733);
        let first: InstrumentId = last + 1;
        let middle: InstrumentId = last + 2;
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", last, 0,
            false, &mut conn, &mut hb,
        );
        farm.also_ask_for_series(last, 756733, &[236], &context, &mut conn, &mut hb);
        farm.note_series_asked_on(last, &[236], 4);
        farm.note_moved(first, middle);
        farm.note_moved(middle, last);
        let _ = super::drain_inner(&mut peer);

        // The withdrawal names the slot its caller was given at the start.
        farm.send_mktdata_unsubscribe(first, 0, 0, &[236], 5, false, &mut conn, &mut hb);

        assert!(
            !farm.asked_generic_ticks.get(&last).is_some_and(|a| a.contains(&236)),
            "the series is given up where its caller was finally sent",
        );
    }

    /// A withdrawal names which occupancy of the slot it was about, even where
    /// the venue never identified the contract.
    ///
    /// A slot is reusable and a contract stated by description has no id, so
    /// neither the slot nor the contract says which occupancy ended. The
    /// request that took the slot states the number it took it under: a
    /// withdrawal decided against the occupancy before this one leaves the
    /// subscription standing, whatever order the two decisions were made in.
    #[test]
    fn a_withdrawal_names_which_occupancy_of_the_slot_it_was_about() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        // The subscription on the slot was asked for by the request that took
        // it under 20.
        farm.note_subscription_began_under(instrument, 20);
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        let _ = super::drain_inner(&mut peer);

        // A withdrawal from the request that held the slot before it, deciding
        // later than 20 and naming no contract of its own.
        farm.send_mktdata_unsubscribe(instrument, 0, 11, &[], u64::MAX, false, &mut conn, &mut hb);
        assert!(
            farm.instrument_md_reqs.iter().any(|(id, _)| *id == instrument),
            "the subscription of the occupancy that holds the slot stands",
        );
        assert!(
            super::drain_inner(&mut peer).is_empty(),
            "and nothing was withdrawn on the wire",
        );

        // And the request that took it can give it up.
        farm.send_mktdata_unsubscribe(instrument, 0, 20, &[], u64::MAX, false, &mut conn, &mut hb);
        assert!(
            !farm.instrument_md_reqs.iter().any(|(id, _)| *id == instrument),
            "the occupancy that took the slot can withdraw it",
        );
    }

    /// A request that joins what is already on a slot does not rename it.
    ///
    /// The venue's one-shot asked for beside a live stream, or a second caller
    /// on the same contract, did not begin that subscription. Renaming it left
    /// the caller that did begin it unable to take its own subscription down:
    /// the slot, the stream and the allowance stayed held for the session.
    #[test]
    fn a_request_that_joins_a_slot_does_not_rename_the_occupancy() {
        let mut farm = FarmState::new();

        farm.note_subscription_began_under(3, 10);
        farm.note_subscription_began_under(3, 11);
        assert_eq!(farm.what_took_it(3), 10, "the one that began it still holds it");

        // Its callers being moved onto it is the one thing that changes hands.
        farm.note_it_changed_hands(3, 12);
        assert_eq!(farm.what_took_it(3), 12, "and the callers that arrived hold it now");
    }

    /// The total a running series was read against goes with the series.
    ///
    /// A running series states a cumulative total and what is published is the
    /// difference between two of them. Kept after the series is withdrawn, the
    /// first reading once somebody asks for it again is measured from the
    /// total the venue stated before it stopped — one print for everything
    /// that traded while nobody was asking, which the venue never stated.
    #[test]
    fn the_total_a_running_series_was_read_against_goes_with_it() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        farm.also_ask_for_series(instrument, 756733, &[233], &context, &mut conn, &mut hb);
        let _ = super::drain_inner(&mut peer);
        // What the venue has stated so far, which the next reading is read
        // against.
        farm.rt_volume_totals.insert((instrument, 48), (1.0, 100, 3));

        farm.stop_asking_for_series(instrument, 0, 0, &[233], u64::MAX, &mut conn, &mut hb);

        assert!(
            !farm.rt_volume_totals.contains_key(&(instrument, 48)),
            "nothing is left for a later reading to be measured from",
        );
    }

    /// A series named while a reconnect is still pacing its way through is
    /// what the subscription asks for when its turn comes.
    ///
    /// The queue holds what has not been sent yet, and a caller joining a
    /// contract that is still in it is accepted. Looked for in the record
    /// alone, its series went nowhere: the call answered, the queued
    /// subscription went out asking for the list as it stood, and the caller
    /// watched for a stream nobody had asked the venue for.
    #[test]
    fn a_series_named_while_the_replay_is_pacing_is_asked_for_when_its_turn_comes() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let mut conn = None;

        // A contract waiting its turn in the paced replay, the way a reconnect
        // leaves it.
        farm.replay_queue.push_back((
            instrument, 756733, "SPY".into(), "SMART".into(), "STK".into(),
            String::new(), 0.0, String::new(), String::new(), 0,
        ));

        farm.also_ask_for_series(instrument, 756733, &[236], &context, &mut conn, &mut hb);

        assert_eq!(
            farm.asked_generic_ticks.get(&instrument).map(Vec::as_slice),
            Some([236u32].as_slice()),
            "the subscription asks for it when the replay reaches it",
        );
    }

    /// A withdrawal decided before the subscription that is up began leaves it
    /// standing.
    ///
    /// A caller decides to withdraw and says so a moment later, and a caller
    /// asking for the same contract in between is answered off the
    /// subscription that is up. The withdrawal that follows named the slot and
    /// nothing else, so it took down a subscription decided against before
    /// that caller had asked for anything — leaving it published as watching a
    /// contract with nothing on the wire.
    #[test]
    fn a_withdrawal_decided_before_the_subscription_began_leaves_it_standing() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.note_subscription_asked_on(instrument, 10);
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        let _ = super::drain_inner(&mut peer);

        // A withdrawal decided before that request was made.
        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], 5, false, &mut conn, &mut hb);
        assert!(
            farm.instrument_md_reqs.iter().any(|(id, _)| *id == instrument),
            "the subscription that was asked for after it stands",
        );
        assert!(
            super::drain_inner(&mut peer).is_empty(),
            "and nothing was withdrawn on the wire",
        );

        // And one decided after it takes it down.
        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], 11, false, &mut conn, &mut hb);
        assert!(
            !farm.instrument_md_reqs.iter().any(|(id, _)| *id == instrument),
            "the caller that asked for this subscription can withdraw it",
        );
    }

    /// A caller asking for one series does not keep every series on the slot.
    ///
    /// Each series answers for itself. Answered for the slot as a whole, a
    /// caller that asked for one of them kept the one another caller had just
    /// given up: the venue went on serving it to callers that never named it,
    /// and the rebuild after a reconnect asked for it again.
    #[test]
    fn a_caller_asking_for_one_series_does_not_keep_every_series() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        farm.also_ask_for_series(instrument, 756733, &[233, 236], &context, &mut conn, &mut hb);
        // One of them asked for again, by a caller that arrived after the
        // withdrawal below was decided.
        farm.note_subscription_asked_on(instrument, 9);
        farm.note_series_asked_on(instrument, &[236], 9);
        let _ = super::drain_inner(&mut peer);

        farm.stop_asking_for_series(instrument, 0, 0, &[233, 236], 5, &mut conn, &mut hb);

        let stated = |msg: &[u8], tag: u32| -> Vec<String> {
            let prefix = format!("{tag}=");
            msg.split(|&b| b == 0x01)
                .filter_map(|field| {
                    std::str::from_utf8(field).ok()?.strip_prefix(prefix.as_str()).map(str::to_string)
                })
                .collect()
        };
        let withdrawn: Vec<String> = super::drain_inner(&mut peer)
            .into_iter()
            .filter(|msg| stated(msg, 263).first().map(String::as_str) == Some("2"))
            .flat_map(|msg| stated(&msg, 264))
            .collect();
        assert_eq!(
            withdrawn, ["233".to_string()],
            "only the series nobody asked for after this was decided: {withdrawn:?}",
        );
        assert!(
            farm.asked_generic_ticks.get(&instrument).is_some_and(|asked| asked.contains(&236)),
            "and the one that was asked for again is still asked for",
        );
    }

    /// A withdrawal the subscription outlived still gives up the series it
    /// carried.
    ///
    /// The subscription stands because another caller asked for the contract
    /// after this withdrawal was decided. What that caller did not ask for is
    /// still this one's to give up: dropped with the withdrawal, the venue
    /// served a series nobody was reading for the life of the subscription.
    #[test]
    fn a_withdrawal_the_subscription_outlived_still_gives_up_its_series() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        farm.also_ask_for_series(instrument, 756733, &[236], &context, &mut conn, &mut hb);
        farm.note_series_asked_on(instrument, &[236], 4);
        // A caller that asked for the contract after the withdrawal below was
        // decided, and asked for no series of its own.
        farm.note_subscription_asked_on(instrument, 10);
        let _ = super::drain_inner(&mut peer);

        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[236], 5, false, &mut conn, &mut hb);

        assert!(
            farm.instrument_md_reqs.iter().any(|(id, _)| *id == instrument),
            "the subscription the later caller is being served off stands",
        );
        assert!(
            !farm.asked_generic_ticks.get(&instrument).is_some_and(|a| a.contains(&236)),
            "and the series nobody else asked for is given up",
        );
    }

    /// A withdrawal decided while a caller is on its way onto the slot leaves
    /// the subscription standing.
    ///
    /// A caller whose contract turns out to live in another slot is moved onto
    /// it, and is not recorded as watching it until it reads the move. A
    /// withdrawal decided in between could not see it: the subscription went,
    /// and that caller arrived on a slot with nothing on the wire and nothing
    /// to say so.
    #[test]
    fn a_withdrawal_decided_while_a_caller_is_moving_in_leaves_the_subscription() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        let _ = super::drain_inner(&mut peer);

        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], u64::MAX, true, &mut conn, &mut hb);

        assert!(
            farm.instrument_md_reqs.iter().any(|(id, _)| *id == instrument),
            "the subscription the arriving caller will be served off stands",
        );
        assert!(
            super::drain_inner(&mut peer).is_empty(),
            "and nothing was withdrawn on the wire",
        );
    }

    /// Every tick that states no length of its own says where it ends, so a
    /// record of one is read and the records behind it in the same message
    /// survive.
    ///
    /// Left out of that second list, such a record was abandoned at — and with
    /// it the quote, the trading status, the venue list and every other series
    /// answering into the same message, for as long as the caller kept asking
    /// for the one series that was not read.
    #[test]
    fn every_tick_that_states_no_length_says_where_it_ends() {
        for tick in NO_LENGTH_TICKS {
            assert_eq!(
                PayloadLength::of(tick), PayloadLength::ToTheEnd,
                "{tick} states no length",
            );
            assert!(
                SELF_DESCRIBING_TICKS.contains(&tick),
                "{tick} states no length and no record of it can be stepped over",
            );
        }
    }

    /// A tick that states its length in two bytes is read that way. Which
    /// ticks do is a property of the tick, not something on the frame.
    #[test]
    fn the_length_form_follows_the_tick() {
        assert_eq!(PayloadLength::of(NEWS_REQUEST_TYPE), PayloadLength::TwoBytes);
        assert_eq!(PayloadLength::of(TRADING_STATUS_REQUEST_TYPE), PayloadLength::OneByte);
        assert_eq!(PayloadLength::of(GREEKS_REQUEST_TYPE), PayloadLength::OneByte);
        assert_eq!(PayloadLength::of(320), PayloadLength::ToTheEnd);

        let payload = vec![3u8; 300];
        let msg = framed_news(11, &payload);
        let mut seen = Vec::new();
        read_generic_ticks(&msg[5..], |_| Some(NEWS_REQUEST_TYPE), |_, record| {
            seen.push(record.payload.len())
        });
        assert_eq!(seen, vec![300]);
    }

}
mod decode_publish_tests {
    use super::super::*;
    use crate::bridge::SharedState;
    use crate::engine::context::Context;
    use crate::protocol::tick_decoder;
    use crate::types::QTY_SCALE;

    pub(super) fn push_bits(bits: &mut Vec<u8>, val: u64, n: usize) {
        for i in (0..n).rev() {
            bits.push(((val >> i) & 1) as u8);
        }
    }

    /// One 35=P body carrying `ticks` for `server_tag`, framed as the farm
    /// connection delivers it.
    pub(super) fn framed_35p(server_tag: u32, ticks: &[(u64, u64, u64)]) -> Vec<u8> {
        let mut bits: Vec<u8> = Vec::new();
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, server_tag as u64, 31);
        for (i, &(tick_type, width, value)) in ticks.iter().enumerate() {
            push_bits(&mut bits, tick_type, 5);
            push_bits(&mut bits, if i < ticks.len() - 1 { 1 } else { 0 }, 1);
            push_bits(&mut bits, width - 1, 2);
            push_bits(&mut bits, 0, 1); // positive
            push_bits(&mut bits, value, (width * 8 - 1) as usize);
        }
        let byte_count = bits.len().div_ceil(8);
        let mut payload = vec![0u8; byte_count];
        for (i, &b) in bits.iter().enumerate() {
            if b == 1 {
                payload[i >> 3] |= 1 << (7 - (i & 7));
            }
        }
        let mut tick_payload = Vec::with_capacity(2 + byte_count);
        tick_payload.push((bits.len() >> 8) as u8);
        tick_payload.push((bits.len() & 0xFF) as u8);
        tick_payload.extend_from_slice(&payload);

        let body_len = 5 + tick_payload.len() + 15;
        let mut msg = format!("8=O\x019={body_len}\x01").into_bytes();
        msg.extend_from_slice(b"35=P\x01");
        msg.extend_from_slice(&tick_payload);
        msg.extend_from_slice(b"\x018349=AABBCCDD\x01");
        msg
    }

    /// The same, with the last field still saying another one follows.
    ///
    /// Which is what a record the venue did not finish sending looks like:
    /// the fields before it read perfectly well, and nothing in them says the
    /// record is short.
    pub(super) fn framed_35p_unterminated(server_tag: u32, ticks: &[(u64, u64, u64)]) -> Vec<u8> {
        let mut bits: Vec<u8> = Vec::new();
        push_bits(&mut bits, 0, 1);
        push_bits(&mut bits, server_tag as u64, 31);
        for &(tick_type, width, value) in ticks {
            push_bits(&mut bits, tick_type, 5);
            push_bits(&mut bits, 1, 1); // another follows, and none does
            push_bits(&mut bits, width - 1, 2);
            push_bits(&mut bits, 0, 1);
            push_bits(&mut bits, value, (width * 8 - 1) as usize);
        }
        let byte_count = bits.len().div_ceil(8);
        let mut payload = vec![0u8; byte_count];
        for (i, &b) in bits.iter().enumerate() {
            if b == 1 {
                payload[i >> 3] |= 1 << (7 - (i & 7));
            }
        }
        let mut tick_payload = Vec::with_capacity(2 + byte_count);
        tick_payload.push((bits.len() >> 8) as u8);
        tick_payload.push((bits.len() & 0xFF) as u8);
        tick_payload.extend_from_slice(&payload);

        let body_len = 5 + tick_payload.len() + 15;
        let mut msg = format!("8=O\x019={body_len}\x01").into_bytes();
        msg.extend_from_slice(b"35=P\x01");
        msg.extend_from_slice(&tick_payload);
        msg.extend_from_slice(b"\x018349=AABBCCDD\x01");
        msg
    }

    /// A record the venue did not finish sending is not a record.
    ///
    /// Running out of bits is not the same as a field saying no more follows,
    /// and the two were told apart by nobody: whatever had been read was
    /// handed over as a whole record. A sidecar cut off before the field that
    /// says what its numbers mean therefore read as the top of the book, and
    /// the day's volume was published as a bid.
    #[test]
    fn a_record_that_ends_early_publishes_nothing() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let id = context.market.register(756733);
        context.market.register_server_tag(9, id);
        context.market.set_min_tick(id, 0.01);

        // Field zero, and then the record stops with more still promised. Under
        // the ordinary layout that number is the bid; under the layout the
        // missing field would have stated, it is not.
        farm.handle_tick_data(
            &framed_35p_unterminated(9, &[(0, 2, 601)]),
            &mut context, &shared, &None,
        );

        assert_eq!(context.market.quote(id).bid, 0, "a number from a record that never ended");
        assert!(
            shared.market.drain_series_ticks(id).is_empty(),
            "and nothing was published beside it either",
        );
    }

    /// A record says what its own fields mean, and this client reads it.
    ///
    /// Field eighteen is a discriminator rather than a value. Absent, the
    /// record is the top of the book. Stating one, the same numbers that are
    /// the last price and the close are the bid's yield and the ask's, and the
    /// two sides move up a place. Stating two, they are the day's volume, its
    /// high, its low and its close.
    ///
    /// Read as the ordinary layout — which is what this client did — a sidecar
    /// published the day's volume as a bid and its high as an ask, and the
    /// yields reached nobody at all.
    #[test]
    fn a_record_is_read_under_the_layout_it_states() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let id = context.market.register(756733);
        context.market.register_server_tag(9, id);
        context.market.set_min_tick(id, 0.01);
        let mts = context.market.min_tick_scaled(id);

        // The two sides and their yields.
        farm.handle_tick_data(
            &framed_35p(9, &[
                (tick_decoder::O_LAYOUT, 1, 1),
                (0, 2, 601),
                (1, 2, 602),
                (2, 2, 4_500),
                (3, 2, 4_600),
            ]),
            &mut context, &shared, &None,
        );
        let q = context.market.quote(id);
        assert_eq!(q.bid, 601 * mts, "the bid moved up a place");
        assert_eq!(q.ask, 602 * mts, "and the ask with it");
        assert_eq!(q.last, 0, "neither of them is the last price");
        let said = shared.market.drain_series_ticks(id);
        // As prices, which is the family these belong to and the callback a
        // caller of the reference client reads them on.
        let yields: Vec<(i32, f64)> = said.iter().filter_map(|t| match t.value {
            crate::types::SeriesValue::Price(v) => Some((t.tick_type, v)),
            _ => None,
        }).collect();
        assert!(
            yields.contains(&(50, 0.45)) && yields.contains(&(51, 0.46)),
            "the two yields, counted in ten thousandths: {yields:?}",
        );
        assert!(
            !said.iter().any(|t| matches!(t.value, crate::types::SeriesValue::Generic(_))),
            "a yield reached the callback the reference client puts no yield on: {said:?}",
        );

        // The day's extremes, on the same numbers.
        farm.handle_tick_data(
            &framed_35p(9, &[
                (tick_decoder::O_LAYOUT, 1, 2),
                (0, 2, 7_000),
                (1, 2, 701),
                (2, 2, 702),
                (3, 2, 703),
            ]),
            &mut context, &shared, &None,
        );
        let q = context.market.quote(id);
        assert_eq!(q.high, 701 * mts, "the high");
        assert_eq!(q.low, 702 * mts, "the low");
        assert_eq!(q.close, 703 * mts, "the close");
        assert_eq!(q.bid, 601 * mts, "and the bid is untouched by a record that states none");

        // And an ordinary record still reads as one.
        farm.handle_tick_data(
            &framed_35p(9, &[(tick_decoder::O_LAST_PRICE, 2, 801)]),
            &mut context, &shared, &None,
        );
        assert_eq!(context.market.quote(id).last, 801 * mts, "the last price");
    }

    /// The constants table says which wire type is which; this says where each
    /// one lands. Nothing else pins that: swapping the open and close arms with
    /// the table intact passes the whole suite, and that is precisely the
    /// failure this decode change exists to remove — two plausible prices
    /// exchanged, with the P&L path reading the wrong one.
    #[test]
    fn each_price_type_lands_in_its_own_quote_field() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let id = context.market.register(756733);
        context.market.register_server_tag(9, id);
        context.market.set_min_tick(id, 0.01);

        // Distinct magnitudes, so no two fields can be confused.
        let msg = framed_35p(9, &[
            (tick_decoder::O_LAST_PRICE, 2, 501),
            (tick_decoder::O_HIGH_PRICE, 2, 502),
            (tick_decoder::O_LOW_PRICE, 2, 503),
            (tick_decoder::O_OPEN_PRICE, 2, 504),
            (tick_decoder::O_CLOSE_PRICE, 2, 505),
        ]);
        farm.handle_tick_data(&msg, &mut context, &shared, &None);

        let mts = context.market.min_tick_scaled(id);
        let q = context.market.quote(id);
        assert_eq!(q.last, 501 * mts, "last");
        assert_eq!(q.high, 502 * mts, "high");
        assert_eq!(q.low, 503 * mts, "low");
        assert_eq!(q.open, 504 * mts, "open");
        assert_eq!(q.close, 505 * mts, "close");
    }

    /// The timestamp arm carries seconds and is stored in nanoseconds, and the
    /// guard is what keeps a date-shaped value out of the field.
    #[test]
    fn the_timestamp_is_seconds_stored_as_nanoseconds() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let id = context.market.register(756733);
        context.market.register_server_tag(11, id);
        context.market.set_min_tick(id, 0.01);

        farm.handle_tick_data(
            &framed_35p(11, &[(tick_decoder::O_TS_BASE, 4, 1_785_325_554)]),
            &mut context, &shared, &None,
        );
        assert_eq!(
            context.market.quote(id).timestamp_ns, 1_785_325_554_000_000_000,
            "an epoch second is stored as nanoseconds",
        );

        // A yyyymmdd-shaped value is not a timestamp and must not land here.
        let id2 = context.market.register(265598);
        context.market.register_server_tag(12, id2);
        context.market.set_min_tick(id2, 0.01);
        farm.handle_tick_data(
            &framed_35p(12, &[(tick_decoder::O_TS_BASE, 4, 20_260_729)]),
            &mut context, &shared, &None,
        );
        assert_eq!(
            context.market.quote(id2).timestamp_ns, 0,
            "a date-shaped magnitude is dropped rather than stored",
        );
    }

    /// The base is stated per stream. Held once for the connection, a base
    /// stated for one instrument was what the next offset on any other added
    /// to, so every stream but the last to state a base carried that one's
    /// second.
    #[test]
    fn a_base_stated_for_one_stream_does_not_stamp_another() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let a = context.market.register(756733);
        context.market.register_server_tag(11, a);
        context.market.set_min_tick(a, 0.01);
        let b = context.market.register(265598);
        context.market.register_server_tag(12, b);
        context.market.set_min_tick(b, 0.01);

        // B states its base, A states a later one, then B moves forward.
        for (tag, tick) in [
            (12, (tick_decoder::O_TS_BASE, 4, 1_785_325_000)),
            (11, (tick_decoder::O_TS_BASE, 4, 1_785_326_000)),
            (12, (tick_decoder::O_TS_OFFSET, 1, 7)),
        ] {
            farm.handle_tick_data(&framed_35p(tag, &[tick]), &mut context, &shared, &None);
        }
        assert_eq!(
            context.market.quote(b).timestamp_ns, 1_785_325_007_000_000_000,
            "B's offset adds to B's own base",
        );
        assert_eq!(
            context.market.quote(a).timestamp_ns, 1_785_326_000_000_000_000,
            "A's stamp is not moved by B's offset",
        );
    }

    /// The pair goes with the slot. Left behind, a contract registered into
    /// a freed slot had its first offset added to the previous occupant's base.
    #[test]
    fn a_freed_slot_carries_no_base_into_its_next_contract() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let a = context.market.register(756733);
        context.market.register_server_tag(11, a);
        context.market.set_min_tick(a, 0.01);
        farm.handle_tick_data(
            &framed_35p(11, &[(tick_decoder::O_TS_BASE, 4, 1_785_325_000)]),
            &mut context, &shared, &None,
        );
        context.market.unregister(a);

        let c = context.market.register(265598);
        assert_eq!(c, a, "the slot is reused");
        context.market.register_server_tag(13, c);
        context.market.set_min_tick(c, 0.01);
        farm.handle_tick_data(
            &framed_35p(13, &[(tick_decoder::O_TS_OFFSET, 1, 7)]),
            &mut context, &shared, &None,
        );
        assert_eq!(
            context.market.quote(c).timestamp_ns, 0,
            "an offset with no base of its own stamps nothing",
        );
    }

    /// The producer half of the quantity contract. Everything downstream
    /// divides by `QTY_SCALE`, so a decode path that stores the wire magnitude
    /// raw delivers quantities 10_000x too small — and nothing else
    /// in the suite reaches this function, which is why that shipped.
    #[test]
    fn decoded_quantities_are_stored_as_fixed_point() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let id = context.market.register(756733);
        context.market.register_server_tag(7, id);
        context.market.set_min_tick(id, 0.01);

        let msg = framed_35p(7, &[
            (tick_decoder::O_BID_SIZE, 1, 42),
            (tick_decoder::O_ASK_SIZE, 1, 17),
            (tick_decoder::O_LAST_SIZE, 1, 5),
            (tick_decoder::O_VOLUME, 2, 1234),
        ]);
        farm.handle_tick_data(&msg, &mut context, &shared, &None);

        let q = context.market.quote(id);
        assert_eq!(q.bid_size, 42 * QTY_SCALE, "bid_size must be stored fixed-point");
        assert_eq!(q.ask_size, 17 * QTY_SCALE, "ask_size must be stored fixed-point");
        assert_eq!(q.last_size, 5 * QTY_SCALE, "last_size must be stored fixed-point");
        assert_eq!(q.volume, 1234 * QTY_SCALE, "volume must be stored fixed-point");
    }

    /// Prices were already scaled correctly; pin that the quantity change did
    /// not disturb them.
    #[test]
    fn decoded_prices_are_still_scaled_by_min_tick() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let id = context.market.register(756733);
        context.market.register_server_tag(9, id);
        context.market.set_min_tick(id, 0.01);
        let mts = context.market.min_tick_scaled(id);

        let msg = framed_35p(9, &[(tick_decoder::O_BID_PRICE, 2, 15000)]);
        farm.handle_tick_data(&msg, &mut context, &shared, &None);

        assert_eq!(context.market.quote(id).bid, 15000 * mts);
    }
}
mod resub_tests {
    use super::super::*;
    use crate::engine::market_state::MarketState;

    /// A disconnect clears `instrument_md_reqs` and keeps `md_resub_info`.
    /// Selecting the reconnect's work from the cleared list re-subscribed
    /// nothing, so the farm came back healthy and delivered no ticks for the
    /// rest of the session.
    ///
    /// Drives the real `handle_disconnect` rather than simulating what it does
    /// — the test-only hook that skips the clearing is what let this survive,
    /// and a hand-written stand-in can drift from the real one the same way.
    #[test]
    fn resub_targets_survive_a_real_disconnect() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut None, &mut hb,
        );
        farm.handle_disconnect(&mut None, &mut context, &None, &crate::bridge::SharedState::new());
        assert!(farm.instrument_md_reqs.is_empty(), "the disconnect clears the request list");

        let targets = farm.take_resub_targets(&context.market);
        assert_eq!(targets.len(), 1, "the subscription must survive the disconnect");
        assert_eq!(targets[0].0, instrument);
        assert_eq!(targets[0].1, 756733, "con_id must be resolved for the re-issue");
        assert_eq!(targets[0].2, "SPY");

        // Re-issuing with no connection must still leave the record standing,
        // so a later reconnect can retry rather than losing the subscription.
        let (id, con_id, sym, exch, st, ltd, k, r, m, mode) = targets.into_iter().next().unwrap();
        farm.send_mktdata_subscribe(
            con_id, &sym, &exch, &st, &ltd, k, &r, &m, id, mode, false, &mut None, &mut hb,
        );
        assert_eq!(farm.md_resub_info.len(), 1, "the record must survive an absent connection");
    }

    /// Everything the connection's own numbers key is dropped with it.
    ///
    /// Seven maps were cleared and three keyed the same way were not. What
    /// removes an entry from those three looks it up by an id the reconnect
    /// has already replaced, so an entry left behind is never named again —
    /// and both of the lists are scanned in full on a path that runs per
    /// acknowledgement and per withdrawal.
    #[test]
    fn nothing_keyed_by_the_old_connection_survives_a_disconnect() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);

        farm.send_depth_subscribe(
            5, 756733, "SMART", "ISLAND", "STK", 10, true, &mut None, &mut hb, &shared,
        );
        // An option is the one kind the venue is asked to model, so it is the
        // one kind that records a modelling request to cancel later.
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "OPT", "20261218", 700.0, "C", "100",
            instrument, 0, false, &mut None, &mut hb,
        );
        // And a quote under a number no contract holds, which is remembered so
        // the warning is said once.
        farm.handle_tick_data(
            &super::decode_publish_tests::framed_35p(
                4242, &[(crate::protocol::tick_decoder::O_BID_PRICE, 2, 15000)],
            ),
            &mut context, &shared, &None,
        );
        assert!(!farm.depth_fanout_exchange.is_empty(), "the depth ask is recorded");
        assert!(!farm.greeks_subs.is_empty(), "so is the modelling ask");
        assert!(!farm.quotes_for_no_one.is_empty(), "so is the unclaimed number");

        farm.handle_disconnect(&mut None, &mut context, &None, &crate::bridge::SharedState::new());

        assert!(
            farm.depth_fanout_exchange.is_empty(),
            "left behind, no later withdrawal names it: {:?}",
            farm.depth_fanout_exchange,
        );
        assert!(farm.greeks_subs.is_empty(), "same, keyed by a replaced id");
        assert!(farm.quotes_for_no_one.is_empty(), "server tags start again");
    }

    /// An unsubscribe issued while the farm is down must still cancel. The
    /// lookup it does first early-returns during an outage, so a record left
    /// standing would be replayed on reconnect as a subscription the caller
    /// had explicitly cancelled.
    #[test]
    fn unsubscribing_while_down_does_not_leave_a_resubscribe_record() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut None, &mut hb,
        );
        farm.handle_disconnect(&mut None, &mut context, &None, &crate::bridge::SharedState::new());
        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], u64::MAX, false, &mut None, &mut hb);

        assert!(
            farm.take_resub_targets(&context.market).is_empty(),
            "a cancelled subscription must not come back on reconnect",
        );
    }

    /// The other side of keeping a slot resident: it has to become releasable
    /// again, or the guard turns a bounded pool into a leak and the instrument
    /// cap becomes cumulative-per-session — the failure exists to
    /// prevent. Every route out of a subscription has to clear all three
    /// references, whether the farm is up or down.
    #[test]
    fn a_slot_becomes_reclaimable_again_once_the_subscription_ends() {
        for down in [false, true] {
            let mut farm = FarmState::new();
            let mut context = Context::new();
            let mut hb = HeartbeatState::new();
            let instrument = context.market.register(756733);

            farm.send_mktdata_subscribe(
                756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
                false, &mut None, &mut hb,
            );
            assert!(farm.holds_market_data(instrument), "subscribed: held");

            if down {
                farm.handle_disconnect(&mut None, &mut context, &None, &crate::bridge::SharedState::new());
                // The record deliberately survives a disconnect, so the slot
                // stays held — that is what makes the resubscribe possible.
                assert!(farm.holds_market_data(instrument), "disconnected: still held");
            }

            farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], u64::MAX, false, &mut None, &mut hb);
            assert!(
                !farm.holds_market_data(instrument),
                "unsubscribed (farm down: {down}): the slot must be releasable",
            );
        }
    }

    /// A reconnect's replay is paced, and the pace must not be taken out of
    /// the engine.
    ///
    /// One thread drives every transport, the heartbeats, the reconnects and
    /// shutdown. Sleeping between bursts stops all of it for as long as the
    /// caller's pacing says, so the book is put back across the passes the
    /// loop is already making instead.
    #[test]
    fn a_paced_replay_does_not_hold_the_engine() {
        use crate::engine::hot_loop::{HeartbeatState, ReplayPacing};

        let mut farm = FarmState::new();
        let mut market = MarketState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let shared = SharedState::new();

        for con_id in 0..5i64 {
            let instrument = market.register(700000 + con_id);
            farm.md_resub_info.push((
                instrument, "SPY".into(), "SMART".into(), "STK".into(), String::new(),
                0.0, String::new(), String::new(), 0,
            ));
        }
        context.market = market;

        // A pace no engine could afford to wait out, and one at a time.
        let replay = ReplayPacing { burst: 1, pace: std::time::Duration::from_secs(30) };

        let (sock, _peer) = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let s = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
            let (p, _) = l.accept().unwrap();
            (s, p)
        };
        let mut conn = Some(Connection::new_raw(sock).unwrap());

        let started = Instant::now();
        farm.replay_queue = farm.take_resub_targets(&context.market).into_iter().collect();
        farm.replay_not_before = None;
        farm.drive_replay(replay, &mut conn, &mut hb);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the replay returned rather than waiting out its own pacing",
        );

        assert_eq!(farm.replay_queue.len(), 4, "one burst went out, the rest are waiting");
        assert!(farm.replay_not_before.is_some(), "and the next burst has a time");

        // Before the pace elapses, nothing more goes out.
        farm.drive_replay(replay, &mut conn, &mut hb);
        assert_eq!(farm.replay_queue.len(), 4, "the pacing is still honoured");

        // With the pace elapsed, the next burst goes.
        farm.replay_not_before = Some(Instant::now());
        farm.drive_replay(replay, &mut conn, &mut hb);
        assert_eq!(farm.replay_queue.len(), 3);

        // And a book that empties stops asking for time.
        while !farm.replay_queue.is_empty() {
            farm.replay_not_before = Some(Instant::now());
            farm.drive_replay(replay, &mut conn, &mut hb);
        }
        assert!(farm.replay_queue.is_empty(), "every subscription was put back");
        assert!(farm.replay_not_before.is_none(), "nothing left to wait for");
        let _ = shared;
    }

    /// A farm that drops again mid-replay must not lose what was still queued.
    ///
    /// A subscription that has been sent records itself again as it goes out,
    /// so the next reconnect finds it. One still waiting was never sent, and
    /// the reconnect rebuilds the queue from that record — so a queue dropped
    /// on disconnect takes those subscriptions with it, and the market data
    /// the caller asked for never comes back, with nothing to say why.
    #[test]
    fn a_second_drop_mid_replay_keeps_what_was_still_waiting() {
        use crate::engine::hot_loop::{HeartbeatState, ReplayPacing};

        let mut farm = FarmState::new();
        let mut market = MarketState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();

        for con_id in 0..4i64 {
            let instrument = market.register(700000 + con_id);
            farm.md_resub_info.push((
                instrument, "SPY".into(), "SMART".into(), "STK".into(), String::new(),
                0.0, String::new(), String::new(), 0,
            ));
        }
        context.market = market;

        let (sock, _peer) = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let s = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
            let (p, _) = l.accept().unwrap();
            (s, p)
        };
        let mut conn = Some(Connection::new_raw(sock).unwrap());

        // One goes out; three are still waiting.
        let replay = ReplayPacing { burst: 1, pace: std::time::Duration::from_secs(30) };
        farm.replay_queue = farm.take_resub_targets(&context.market).into_iter().collect();
        farm.drive_replay(replay, &mut conn, &mut hb);
        assert_eq!(farm.replay_queue.len(), 3);

        // And the farm goes before the rest of them do.
        farm.handle_disconnect(&mut None, &mut context, &None, &crate::bridge::SharedState::new());

        assert!(farm.replay_queue.is_empty(), "nothing is left holding them");
        assert_eq!(
            farm.md_resub_info.len(), 4,
            "all four are recorded for the next reconnect: the one that was \
             sent recorded itself, and the three that were not are put back",
        );
    }

    /// A slot reclaimed while the farm was down has no con_id to subscribe.
    #[test]
    fn resub_targets_skip_an_instrument_reclaimed_while_down() {
        let mut farm = FarmState::new();
        let mut market = MarketState::new();
        let instrument = market.register(756733);
        farm.md_resub_info.push((
            instrument, "SPY".into(), "SMART".into(), "STK".into(), String::new(),
            0.0, String::new(), String::new(), 0,
        ));
        market.unregister(instrument);

        assert!(farm.take_resub_targets(&market).is_empty());
    }

    /// The window the two tests below do not reach: between a reconnect and the
    /// last replay burst. `take_resub_targets` empties `md_resub_info` into
    /// `replay_queue` and `instrument_md_reqs` is not refilled until each
    /// subscription is sent, so an instrument waiting its turn is written down
    /// there and nowhere else. Read as free, its slot is handed to another
    /// contract and the replay then binds this contract's server tag and
    /// minimum tick onto that one.
    #[test]
    fn an_instrument_waiting_in_the_replay_queue_is_not_reclaimable() {
        let mut farm = FarmState::new();
        let mut market = MarketState::new();
        let instrument = market.register(756733);
        farm.replay_queue.push_back((
            instrument, 756733, "SPY".into(), "SMART".into(), "STK".into(), String::new(),
            0.0, String::new(), String::new(), 0,
        ));

        assert!(
            farm.holds_market_data(instrument),
            "a subscription still to be replayed must keep the slot resident",
        );
    }

    /// And the caller's withdrawal reaches it there. Left in the queue, the
    /// replay re-sends a subscription that was explicitly cancelled.
    #[test]
    fn withdrawing_reaches_a_subscription_waiting_to_be_replayed() {
        let mut farm = FarmState::new();
        let mut market = MarketState::new();
        let instrument = market.register(756733);
        farm.replay_queue.push_back((
            instrument, 756733, "SPY".into(), "SMART".into(), "STK".into(), String::new(),
            0.0, String::new(), String::new(), 0,
        ));

        // No transport: the withdrawal has to reach the queue whether or not a
        // cancel can go out, which is the case an unsubscribe during an outage
        // already relies on.
        let mut hb = HeartbeatState::new();
        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], u64::MAX, false, &mut None, &mut hb);

        assert!(
            farm.replay_queue.is_empty(),
            "the replay would re-send a subscription the caller withdrew",
        );
        assert!(!farm.holds_market_data(instrument));
    }

    /// The case the test above does not reach: the slot is not merely freed but
    /// handed to another contract before the reconnect. `md_resub_info` holds
    /// no con_id of its own, so the record is combined with whatever con_id the
    /// id now resolves to — the old contract's descriptor subscribing the new
    /// contract's instrument. The guard is that a slot holding market-data
    /// state is not reclaimable in the first place.
    #[test]
    fn an_instrument_holding_a_resubscribe_record_is_not_reclaimable() {
        let mut farm = FarmState::new();
        let mut market = MarketState::new();
        let instrument = market.register(756733);
        farm.md_resub_info.push((
            instrument, "SPY".into(), "SMART".into(), "STK".into(), String::new(),
            0.0, String::new(), String::new(), 0,
        ));

        assert!(
            farm.holds_market_data(instrument),
            "the record alone must keep the slot resident",
        );

        // And a live subscription does the same on its own.
        let mut farm = FarmState::new();
        farm.instrument_md_reqs.push((instrument, MdReqRecord {
            con_id: 756733,
            sec_type: "CS".into(),
            mode_9887: 0,
            entries: vec![MdReqEntry { req_id: 7, request_type: 442, venue: "BEST".into() }],
        }));
        assert!(farm.holds_market_data(instrument), "a live subscription");

        // An instrument with none of the three is free to go.
        assert!(!FarmState::new().holds_market_data(instrument));
    }

    /// And the chargeable snapshot is where the two questions come apart.
    ///
    /// It holds the slot, because the slot must not go back to the table while
    /// one is out on it. It is not a subscription anybody can be given instead
    /// of their own: it is withdrawn the moment it completes and it is never
    /// recorded for replay, so a subscribe pointed at it was never sent and
    /// the withdrawal then took the record it was pointed at. The caller was
    /// left holding a number that reads as subscribed with nothing on it.
    ///
    /// Built by the subscribe itself rather than by hand. A record written out
    /// here states only what the test thought of, and a subscription registers
    /// more than its quote — the trading status and the exchange map ride
    /// beside whichever kind was asked for. Read as "anything that is not the
    /// snapshot's own number", those companions answered for a stream that was
    /// never asked for, and the guard below was inert against the case it
    /// exists for.
    #[test]
    fn a_snapshot_holds_the_slot_and_is_not_a_stream_to_follow() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        let mut hb = HeartbeatState::new();

        // A snapshot and nothing else, sent the way the engine sends one.
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "",
            instrument, 0, true, &mut None, &mut hb,
        );

        assert!(
            farm.holds_market_data(instrument),
            "the slot is in use and cannot be reclaimed under the snapshot",
        );
        assert!(
            !farm.holds_a_stream(instrument),
            "a subscribe told to follow a snapshot is never sent, and the \
             snapshot's own withdrawal takes the record with it",
        );

        // And an ordinary subscription on the same contract is one.
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "",
            instrument, 0, false, &mut None, &mut hb,
        );
        assert!(farm.holds_a_stream(instrument), "a live subscription");
    }
}
use std::collections::HashMap;

fn tag_values(tags: &[(u32, String)], tag: u32) -> Vec<&str> {
    tags.iter().filter(|(t, _)| *t == tag).map(|(_, v)| v.as_str()).collect()
}

/// The server routes a market-data subscription by SecurityType and
/// Exchange even when a conId is supplied. Describing every contract as a
/// SMART-routed common stock makes the server ack only the trade leg of a
/// futures subscription, so bid/ask never arrives.
#[test]
fn conid_subscribe_describes_the_actual_contract() {
    let fut = build_conid_subscribe_tags(true, false, 1, 2, 793356225, "CME", "FUT", 0, "T", &[]);
    assert_eq!(tag_values(&fut, 167), ["FUT", "FUT"], "SecurityType must say FUT");
    assert_eq!(tag_values(&fut, 207), ["CME", "CME"], "Exchange must say CME");

    // Both legs of the realtime fan-out are requested: 442 bid/ask, 443 last.
    assert_eq!(tag_values(&fut, 264), ["442", "443"]);
    assert_eq!(tag_values(&fut, 262), ["1", "2"]);
    assert_eq!(tag_values(&fut, 146), ["2"]);
}

/// The chargeable snapshot is its own request type asked for under its own
/// action: one entry whatever the feed, and no 9887 beside it, which selects
/// between the feeds a stream is served from. Pinned because the venue names
/// this type back when it refuses one for want of the entitlement, so the
/// number is the venue's rather than this client's.
#[test]
fn the_chargeable_snapshot_is_its_own_request_type() {
    let snap = build_conid_subscribe_tags(true, true, 1, 2, 265598, "SMART", "STK", 0, "T", &[]);
    assert_eq!(
        snap,
        vec![
            (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
            (fix::TAG_SENDING_TIME, "T".to_string()),
            (263, "3".to_string()),
            (146, "1".to_string()),
            (262, "1".to_string()),
            (6008, "265598".to_string()),
            (207, "BEST".to_string()),
            (167, "CS".to_string()),
            (264, "624".to_string()),
            (6088, "Socket".to_string()),
            (9830, "1".to_string()),
            (9839, "1".to_string()),
        ],
    );

    // And a feed named beside it does not turn it back into a stream.
    let frozen = build_conid_subscribe_tags(false, true, 1, 2, 265598, "SMART", "STK", 2, "T", &[]);
    assert_eq!(tag_values(&frozen, 264), ["624"]);
    assert_eq!(tag_values(&frozen, 146), ["1"]);
    assert!(tag_values(&frozen, 9887).is_empty(), "no feed is named beside it");
}

/// An ordinary snapshot is a subscription this client ends, not a request type
/// of its own: the venue is asked to subscribe, exactly as for a stream, and
/// the request is withdrawn once every kind a snapshot is made of has arrived.
#[test]
fn an_ordinary_snapshot_is_asked_for_as_a_subscription() {
    let ordinary = build_conid_subscribe_tags(true, false, 1, 2, 265598, "SMART", "STK", 0, "T", &[]);
    assert_eq!(tag_values(&ordinary, 263), ["1"]);
    assert_eq!(tag_values(&ordinary, 264), ["442", "443"]);
}

/// Stocks keep the exact wire shape they had before: SMART maps to BEST and
/// STK to CS, so this path is unchanged for equities. Pinned as the whole
/// ordered tag list rather than the two mapped tags, so a reordering or a
/// dropped field is caught here too.
#[test]
fn conid_subscribe_is_unchanged_for_stocks() {
    let stk = build_conid_subscribe_tags(true, false, 1, 2, 265598, "SMART", "STK", 0, "T", &[]);
    assert_eq!(
        stk,
        vec![
            (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
            (fix::TAG_SENDING_TIME, "T".to_string()),
            (263, "1".to_string()),
            (146, "2".to_string()),
            (262, "1".to_string()),
            (6008, "265598".to_string()),
            (207, "BEST".to_string()),
            (167, "CS".to_string()),
            (264, "442".to_string()),
            (6088, "Socket".to_string()),
            (9830, "1".to_string()),
            (9839, "1".to_string()),
            (262, "2".to_string()),
            (6008, "265598".to_string()),
            (207, "BEST".to_string()),
            (167, "CS".to_string()),
            (264, "443".to_string()),
            (6088, "Socket".to_string()),
            (9830, "1".to_string()),
            (9839, "1".to_string()),
        ],
    );

    let delayed = build_conid_subscribe_tags(false, false, 1, 2, 265598, "SMART", "STK", 3, "T", &[]);
    assert_eq!(
        delayed,
        vec![
            (fix::TAG_MSG_TYPE, fix::MSG_MARKET_DATA_REQ.to_string()),
            (fix::TAG_SENDING_TIME, "T".to_string()),
            (263, "1".to_string()),
            (146, "2".to_string()),
            (262, "1".to_string()),
            (6008, "265598".to_string()),
            (207, "BEST".to_string()),
            (167, "CS".to_string()),
            (264, "442".to_string()),
            (6088, "Socket".to_string()),
            (9830, "1".to_string()),
            (9839, "1".to_string()),
            (9887, "3".to_string()),
            (262, "2".to_string()),
            (6008, "265598".to_string()),
            (207, "BEST".to_string()),
            (167, "CS".to_string()),
            (264, "443".to_string()),
            (6088, "Socket".to_string()),
            (9830, "1".to_string()),
            (9839, "1".to_string()),
            (9887, "3".to_string()),
        ],
    );
}

/// A contract reaches the wire described, or it does not reach it.
///
/// The engine fills a caller's blanks from the venue's own definition and
/// reports the subscription where neither says what the contract is, so this
/// builder is only ever handed a description. Both fields go on the wire as
/// given: a subscription that states neither is answered with nothing, and one
/// that states a guess subscribes to some other instrument under this id.
#[test]
fn conid_subscribe_states_the_description_it_is_given() {
    let fut = build_conid_subscribe_tags(true, false, 1, 2, 793356225, "CME", "FUT", 0, "T", &[]);
    assert_eq!(tag_values(&fut, 167), ["FUT", "FUT"]);
    assert_eq!(tag_values(&fut, 207), ["CME", "CME"]);

    let stk = build_conid_subscribe_tags(true, false, 1, 2, 265598, "SMART", "STK", 0, "T", &[]);
    assert_eq!(tag_values(&stk, 167), ["CS", "CS"]);
    assert_eq!(tag_values(&stk, 207), ["BEST", "BEST"]);
    assert_ne!(fut, stk, "a future is not sent as a stock");
}

/// A delayed or frozen stream asks for both legs, the same two a realtime one
/// asks for, and names its feed beside each.
///
/// Asked for as the single top instead, the subscription carries no number
/// for what last traded: the venue's answer to that half has no request to
/// arrive under, so a caller watching a delayed contract sees bid and ask
/// move while the last price, size and time stay where they were.
#[test]
fn a_delayed_stream_asks_for_both_legs() {
    for mode in [1, 2, 3] {
        let delayed =
            build_conid_subscribe_tags(false, false, 7, 8, 265598, "SMART", "STK", mode, "T", &[]);
        assert_eq!(tag_values(&delayed, 262), ["7", "8"], "both legs are numbered");
        assert_eq!(tag_values(&delayed, 264), ["442", "443"]);
        assert_eq!(tag_values(&delayed, 146), ["2"]);
        assert_eq!(
            tag_values(&delayed, 9887), [mode.to_string(), mode.to_string()],
            "the feed is named beside each leg",
        );
    }

    let realtime = build_conid_subscribe_tags(true, false, 7, 8, 265598, "SMART", "STK", 0, "T", &[]);
    assert!(tag_values(&realtime, 9887).is_empty(), "realtime carries no 9887");
    assert_eq!(tag_values(&realtime, 264), ["442", "443"]);
}

/// Every entry must be self-contained: the server reads conId per entry.
#[test]
fn each_entry_carries_its_own_conid() {
    let fut = build_conid_subscribe_tags(true, false, 1, 2, 793356225, "CME", "FUT", 0, "T", &[]);
    assert_eq!(tag_values(&fut, 6008), ["793356225", "793356225"]);

    let counts: HashMap<u32, usize> =
        fut.iter().fold(HashMap::new(), |mut m, (t, _)| { *m.entry(*t).or_insert(0) += 1; m });
    for tag in [262, 6008, 207, 167, 264, 6088, 9830, 9839] {
        assert_eq!(counts[&tag], 2, "tag {tag} must appear once per entry");
    }
}
mod stale_ack_tests {
    use super::super::*;
    use crate::engine::context::Context;

    /// A `35=Q` in flight when the unsubscribe goes out resolves its request
    /// id before the slot can be reclaimed. Resolving afterwards would bind its
    /// server tag and minTick onto whichever contract took the slot, scaling
    /// that contract's prices by the previous one's tick size.
    #[test]
    fn a_late_ack_for_an_unsubscribed_request_is_ignored() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut None, &mut hb,
        );
        let pending: Vec<u32> = farm.md_req_to_instrument.iter().map(|(r, _)| *r).collect();
        assert!(!pending.is_empty(), "the subscribe must register at least one request");

        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], u64::MAX, false, &mut None, &mut hb);

        for req_id in pending {
            assert!(
                !farm.md_req_to_instrument.iter().any(|(r, _)| *r == req_id),
                "request {req_id} must not resolve after its unsubscribe",
            );
        }
    }
}
mod price_scaling_tests {
    use super::super::*;
    use crate::bridge::SharedState;
    use crate::engine::context::Context;
    use crate::protocol::tick_decoder;

    fn push(bits: &mut Vec<u8>, val: u64, n: usize) {
        for i in (0..n).rev() {
            bits.push(((val >> i) & 1) as u8);
        }
    }

    /// One 35=P body carrying a single extended entry, framed as the farm
    /// connection delivers it. The extended header carries a full byte width,
    /// which is how a magnitude large enough to overflow the price scaling
    /// arrives from the wire.
    fn framed_extended(server_tag: u32, tick_type: u64, byte_width: u64, value: u64) -> Vec<u8> {
        let mut bits: Vec<u8> = Vec::new();
        push(&mut bits, 0, 1);
        push(&mut bits, server_tag as u64, 31);
        push(&mut bits, 31, 5); // extended sentinel
        push(&mut bits, 0, 1);  // has_more
        push(&mut bits, 0, 2);  // raw width, ignored for extended
        push(&mut bits, tick_type, 8);
        push(&mut bits, byte_width, 8);
        push(&mut bits, 0, 1);  // sign
        push(&mut bits, value, (byte_width * 8 - 1) as usize);

        let byte_count = bits.len().div_ceil(8);
        let mut payload = vec![0u8; byte_count];
        for (i, &b) in bits.iter().enumerate() {
            if b == 1 {
                payload[i >> 3] |= 1 << (7 - (i & 7));
            }
        }
        let mut tick_payload = Vec::with_capacity(2 + byte_count);
        tick_payload.push((bits.len() >> 8) as u8);
        tick_payload.push((bits.len() & 0xFF) as u8);
        tick_payload.extend_from_slice(&payload);

        let body_len = 5 + tick_payload.len() + 15;
        let mut msg = format!("8=O\x019={body_len}\x01").into_bytes();
        msg.extend_from_slice(b"35=P\x01");
        msg.extend_from_slice(&tick_payload);
        msg.extend_from_slice(b"\x018349=AABBCCDD\x01");
        msg
    }

    /// A magnitude the price scaling cannot represent must leave the previous
    /// quote standing. Wrapping it publishes an arbitrary price — the probe
    /// for this test produces -1000000, a negative price indistinguishable
    /// downstream from a real quote.
    #[test]
    fn a_price_that_cannot_be_scaled_does_not_replace_the_quote() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let id = context.market.register(756733);
        context.market.register_server_tag(7, id);
        context.market.set_min_tick(id, 0.01);

        farm.handle_tick_data(
            &framed_extended(7, tick_decoder::O_LAST_PRICE, 2, 15_000),
            &mut context, &shared, &None,
        );
        let good = context.market.quote(id).last;
        assert!(good > 0, "the ordinary tick must land");

        farm.handle_tick_data(
            &framed_extended(7, tick_decoder::O_LAST_PRICE, 8, u64::MAX >> 1),
            &mut context, &shared, &None,
        );
        assert_eq!(
            context.market.quote(id).last, good,
            "an unrepresentable price must be dropped, leaving the last good quote",
        );
    }

    /// A price too large to scale leaves the quote unchanged, so no tick is
    /// announced for it.
    #[test]
    fn a_price_that_cannot_be_scaled_announces_no_tick() {
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let sink = Some(crate::engine::hot_loop::EventSink::new(
            tx,
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        ));
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let id = context.market.register(756733);
        context.market.register_server_tag(7, id);
        context.market.set_min_tick(id, 0.01);

        farm.handle_tick_data(
            &framed_extended(7, tick_decoder::O_LAST_PRICE, 2, 15_000),
            &mut context, &shared, &sink,
        );
        assert_eq!(rx.try_iter().count(), 1, "the ordinary tick is announced");

        farm.handle_tick_data(
            &framed_extended(7, tick_decoder::O_LAST_PRICE, 8, u64::MAX >> 1),
            &mut context, &shared, &sink,
        );
        assert_eq!(rx.try_iter().count(), 0, "the refused one is not");
    }

    const FRAME: &[u8] = &[0x7e, 0xf7, 0x20, 0x01, 0x40, 0x57, 0x04, 0x41, 0xc8, 0xf2, 0xf3, 0x45, 0x3f, 0xef, 0xfc, 0x3a, 0xab, 0x98, 0x37, 0xb3, 0x3f, 0x12, 0xf3, 0x0c, 0x1b, 0xcf, 0xac, 0xe7, 0x3f, 0x53, 0x13, 0xaf, 0x03, 0xfc, 0x00, 0x00, 0xbf, 0xa0, 0x60, 0x85, 0xf4, 0x8d, 0x38, 0x00, 0x40, 0x0d, 0x23, 0xdb, 0x03, 0xb8, 0xf5, 0x14, 0x40, 0x71, 0x7c, 0xb2, 0x05, 0x82, 0x74, 0xf0, 0x3f, 0xf0, 0x07, 0x27, 0xcf, 0x01, 0x13, 0xef, 0x40, 0x73, 0x7f, 0x52, 0x20, 0x00, 0x00, 0x00, 0x3f, 0x9f, 0xf2, 0x61, 0x35, 0xdd, 0x42, 0xd9, 0x40, 0x2e, 0x2b, 0xd8, 0x8e, 0x99, 0xfa, 0xb0, 0x3f, 0x1e, 0x54, 0x91, 0xb1, 0x1c, 0x9a, 0x6c, 0xbe, 0xf5, 0x34, 0xf6, 0xa2, 0xc8, 0x61, 0xb4, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3f, 0xb5, 0x32, 0x2a, 0x5c, 0xf4, 0xd4];

    /// A frame the venue sent for a deep in-the-money call, byte for byte.
    /// Nothing here is constructed: a wrong alignment does not produce a price
    /// that decomposes into the other two fields by accident.
    #[test]
    fn the_venue_states_an_option_model() {
        let c = super::super::decode_greeks(FRAME).expect("the payload is stated valid");
        assert!((c.opt_price - 92.066_515_195_137_14).abs() < 1e-9, "{c:?}");
        assert!((c.delta - 0.999_539_694_925_024_9).abs() < 1e-12, "deep in the money: {c:?}");
        assert!((c.gamma - 0.000_072_286_237_766_827_99).abs() < 1e-15, "{c:?}");
        assert!((c.vega - 0.001_164_360_917_698_559_2).abs() < 1e-15, "{c:?}");
        assert!((c.theta - -0.031_986_414_053_434_94).abs() < 1e-12, "{c:?}");
        assert!((c.und_price - 311.957_550_048_828_1).abs() < 1e-9, "{c:?}");
        // The wire carries this over one of the days it counts beside it, and
        // it is handed on over a year: the venue's 0.0311980427862320 reads as
        // 0.596, which is what a volatility of sixty per cent looks like on a
        // contract a day and a half from expiring. Left as the wire carries
        // it, every volatility this client reports is short by the root of a
        // year.
        assert!(
            (c.implied_vol - 0.031_198_042_786_232_037 * 365.0_f64.sqrt()).abs() < 1e-12,
            "{c:?}",
        );
        // The strike was 220, so the model price sits just above the
        // intrinsic. A mis-read of the layout does not land there.
        let intrinsic = c.und_price - 220.0;
        assert!(c.opt_price > intrinsic, "worth at least its intrinsic: {c:?}");
        assert!(c.opt_price - intrinsic < 1.0, "and barely more, this close to expiry: {c:?}");
        assert_eq!(c.pv_dividend, f64::MAX, "not stated on this tick");
    }

    /// The same frame, read for the figures the documented callback has no
    /// room for.
    ///
    /// Eight fields reach a caller through `tickOptionComputation` and the
    /// venue states eighteen on this tick. The rest were read only to step the
    /// cursor past them and then dropped on the floor — the one first-order
    /// greek the reference cannot answer among them.
    ///
    /// The cross-checks are what make this a reading rather than a
    /// transcription: a call this deep in the money is expected to be
    /// exercised long before it expires, and the price at which exercising
    /// beats holding sits between the strike and the underlying.
    #[test]
    fn the_venue_states_more_of_its_model_than_the_callback_carries() {
        let c = super::super::decode_greeks(FRAME).expect("the payload is stated valid");

        assert!((c.fugit - 3.642_507_580_835_294_7).abs() < 1e-12, "{c:?}");
        assert!(
            c.fugit < c.cal_days,
            "a call at a delta of {} is exercised before it expires: {c:?}", c.delta,
        );

        assert!((c.exercise_boundary - 279.793_462_285_611).abs() < 1e-9, "{c:?}");
        assert!(
            220.0 < c.exercise_boundary && c.exercise_boundary < c.und_price,
            "the price that makes exercising worth more than holding sits between \
             the strike and the underlying: {c:?}",
        );

        assert!((c.forward_coeff - 1.001_746_948_824_116_6).abs() < 1e-12, "{c:?}");
        assert!((c.model_yield - -2.022_446_476_432_496_8e-5).abs() < 1e-18, "{c:?}");
        assert_eq!(c.bridge_yield, 0.0, "stated, and stated as nothing");

        // The venue left the rate greek off this frame: its flag is clear, and
        // a figure nobody stated is not a figure of nought.
        assert_eq!(c.rho, f64::MAX, "not stated on this tick");
        // And this body ends before the last figure its flags name, which the
        // walk marks unstated rather than reading past the end.
        assert_eq!(c.time_value, f64::MAX, "the body ends before it");
    }

    /// A payload the venue did not mark valid carries no numbers.
    #[test]
    fn an_invalid_option_model_states_nothing() {
        assert!(super::super::decode_greeks(&[0u8; 32]).is_none());
        assert!(super::super::decode_greeks(&[0xff, 0xff, 0xff, 0xfe]).is_none(), "too short to hold one");
    }

    /// A subscription that asks for the option model has to withdraw it too.
    /// Left behind, the venue keeps sending a model for a contract the caller
    /// stopped watching, and nothing holds a request id to stop it by.
    #[test]
    fn cancelling_an_option_withdraws_its_model() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let instrument = context.market
            .try_register_contract(805711629, "AAPL", "OPT", "SMART", "20260821|220|C|100")
            .unwrap();
        let mut conn = None;
        let mut hb = HeartbeatState::new();
        farm.send_mktdata_subscribe(
            805711629, "AAPL", "SMART", "OPT", "20260821", 220.0, "C", "100",
            instrument, 0, false, &mut conn, &mut hb,
        );
        assert_eq!(farm.greeks_subs.len(), 1, "an option is worth modelling");
        let record = &farm.instrument_md_reqs.iter()
            .find(|(id, _)| *id == instrument).expect("its requests").1;
        assert!(
            record.entries.iter().any(|e| e.req_id == farm.greeks_subs[0].0),
            "and the model is one of them, so a cancel finds it",
        );

        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], u64::MAX, false, &mut conn, &mut hb);
        assert!(farm.greeks_subs.is_empty(), "withdrawn with the rest");
        assert!(farm.instrument_md_reqs.iter().all(|(id, _)| *id != instrument));
    }

    /// Anything without a volatility to imply is not asked to be modelled: the
    /// venue answers such a request with nothing at all.
    #[test]
    fn a_stock_is_not_asked_for_an_option_model() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let instrument = context.market
            .try_register_contract(756733, "SPY", "STK", "SMART", "").unwrap();
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "",
            instrument, 0, false, &mut None, &mut HeartbeatState::new(),
        );
        assert!(farm.greeks_subs.is_empty());
    }
}
mod trading_status_subscribe_tests {
    use super::super::build_trading_status_subscribe_tags;

    /// The trading status is its own subscription, named by its own tick where
    /// a price subscription names a request type.
    #[test]
    fn the_status_is_asked_for_by_its_own_tick() {
        let tags = build_trading_status_subscribe_tags(7, 756733, "STK", "SMART", "20260810-12:00:00");
        let get = |t: u32| tags.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_str());
        assert_eq!(get(264), Some("437"), "its own tick, not a request type");
        assert_eq!(get(262), Some("7"), "under the request the prices came under");
        assert_eq!(get(6008), Some("756733"));
    }

    /// It names the contract's own exchange. The option model and the news feed
    /// go by names of their own; everything else is asked for where it trades,
    /// and naming a stand-in here asks a venue that does not list the contract.
    #[test]
    fn it_names_the_exchange_the_contract_trades_on() {
        let tags = build_trading_status_subscribe_tags(1, 1, "STK", "ARCA", "t");
        let venue = tags.iter().find(|(k, _)| *k == 207).map(|(_, v)| v.as_str());
        assert_eq!(venue, Some("ARCA"), "not a stand-in");
    }

    /// And names it the way the prices beside it name it: the wire's own
    /// spelling of the venue, the smart route where the caller named none. The
    /// caller's spelling reaches nothing — the legacy name for Nasdaq routes
    /// nowhere, and a blank venue is answered with nothing at all.
    #[test]
    fn it_names_the_venue_the_way_the_wire_spells_it() {
        let venue = |exchange: &str| {
            build_trading_status_subscribe_tags(1, 1, "STK", exchange, "t")
                .into_iter()
                .find(|(k, _)| *k == 207)
                .map(|(_, v)| v)
                .expect("the venue is always stated")
        };
        assert_eq!(venue("SMART"), "BEST", "the wire's name for the smart route");
        assert_eq!(venue(""), "BEST", "a caller naming no venue means the smart route");
        assert_eq!(venue("ISLAND"), "NASDAQ", "the legacy spelling routes nowhere");
    }

    /// So the entry written down against each companion names the venue it went
    /// out on. Recorded under one name and asked under another, the withdrawal
    /// states a venue the subscription never named and the venue leaves it being
    /// served.
    #[test]
    fn the_companions_are_recorded_under_the_venue_they_go_out_on() {
        use super::super::*;

        for exchange in ["SMART", "", "ISLAND", "ARCA"] {
            let mut farm = FarmState::new();
            let mut context = Context::new();
            let mut hb = HeartbeatState::new();
            let instrument = context.market.register(756733);

            farm.send_mktdata_subscribe(
                756733, "SPY", exchange, "STK", "", 0.0, "", "", instrument, 0,
                false, &mut None, &mut hb,
            );

            let tags = build_trading_status_subscribe_tags(1, 756733, "STK", exchange, "t");
            let asked_on = tags.iter().find(|(k, _)| *k == 207).map(|(_, v)| v.as_str()).unwrap();
            let (_, record) = farm.instrument_md_reqs.iter()
                .find(|(id, _)| *id == instrument)
                .expect("the subscription is recorded");
            let recorded: Vec<&str> = record.entries.iter()
                .filter(|e| e.request_type == TRADING_STATUS_REQUEST_TYPE
                    || e.request_type == BBO_EXCHANGE_MAP_REQUEST_TYPE)
                .map(|e| e.venue.as_str())
                .collect();
            assert_eq!(recorded.len(), 2, "the status and the exchange map both ride along");
            for venue in recorded {
                assert_eq!(
                    venue, asked_on,
                    "exchange {exchange:?}: withdrawn on a venue it was never asked on",
                );
            }
        }
    }
}
mod depth_identity_tests {
    use super::super::*;

    /// A subscription registered as the ack path registers one.
    fn acknowledged(farm: &mut FarmState, stag: u32, caller: u32, venue: &str) {
        farm.depth_tag_to_req.push((stag, caller, true, 0.01, 1.0, venue.to_string()));
    }

    /// The venue echoes back the id it was asked under, so an id taken from
    /// the caller cannot be told apart from one this client allocated. Every
    /// book is asked for under an id this client allocated, and mapped back.
    #[test]
    fn a_callers_id_is_never_what_the_venue_is_asked_under() {
        let mut farm = FarmState::new();
        let shared = SharedState::new();
        let mut conn = None;
        let mut hb = HeartbeatState::new();

        // Two callers, numbered as callers number things.
        farm.send_depth_subscribe(1, 756733, "IEX", "", "STK", 10, false, &mut conn, &mut hb, &shared);
        farm.send_depth_subscribe(2, 756733, "ARCA", "", "STK", 10, false, &mut conn, &mut hb, &shared);

        let asked_under: Vec<u32> = farm.depth_fanout_map.iter().map(|(sub, _)| *sub).collect();
        assert_eq!(asked_under.len(), 2, "one subscription each");
        assert_ne!(asked_under[0], asked_under[1], "and each under its own id");
        for (sub, caller) in &farm.depth_fanout_map {
            let venue = farm.depth_fanout_exchange.iter()
                .find(|(s, _)| s == sub)
                .map(|(_, v)| v.as_str())
                .expect("every subscription names the venue it stands on");
            match caller {
                1 => assert_eq!(venue, "IEX"),
                2 => assert_eq!(venue, "ARCA"),
                other => panic!("a caller nobody asked for: {other}"),
            }
        }
    }

    /// A refused book leaves nothing behind. Only the map from the wire id
    /// to the caller was dropped: the two records beside it stayed for the
    /// life of the connection, scanned on every acknowledgement and every
    /// subscribe, and a later acknowledgement of that wire id would have
    /// filed the book under the wire number as though a caller held it.
    #[test]
    fn a_refused_book_leaves_no_record_behind() {
        let mut farm = FarmState::new();
        let shared = SharedState::new();
        let context = Context::new();
        let mut hb = HeartbeatState::new();
        farm.send_depth_subscribe(1, 756733, "ISLAND", "", "STK", 10, false, &mut None, &mut hb, &shared);
        let under = farm.depth_fanout_map[0].0;
        let refused = crate::protocol::fix::fix_build(&[
            (crate::protocol::fix::TAG_MSG_TYPE, "j"),
            (262, &under.to_string()),
            (58, "Error&ISLAND/DEPTH/not available"),
        ], 1);
        farm.handle_subscription_reject(&refused, &context, &shared);
        assert!(farm.depth_fanout_map.is_empty(), "the map goes");
        assert!(farm.depth_subs.is_empty(), "and the wire record: {:?}", farm.depth_subs);
        assert!(farm.depth_fanout_exchange.is_empty(), "and the venue it stood on: {:?}", farm.depth_fanout_exchange);
        assert_eq!(shared.reference.drain_historical_errors().len(), 1, "the caller is told once");
    }

    /// The venue answers a second subscription on a contract and venue it is
    /// already streaming with the tag it is already using.
    #[test]
    fn one_venue_stream_reaches_every_caller_subscribed_to_it() {
        let mut farm = FarmState::new();
        acknowledged(&mut farm, 717550, 1, "IEX");
        acknowledged(&mut farm, 717550, 2, "IEX");
        acknowledged(&mut farm, 990000, 3, "ARCA");

        let both = farm.depth_subscribers_of(717550);
        assert_eq!(both.len(), 2, "a level on this tag belongs to both");
        assert_eq!(both[0].0, 1);
        assert_eq!(both[1].0, 2);
        assert!(both.iter().all(|(_, _, venue)| venue == "IEX"));

        let one = farm.depth_subscribers_of(990000);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].0, 3);
    }

    /// A caller that asked for a shallow book is not handed a deep one.
    ///
    /// The depth is not on the wire. The venue sends the levels it has, and the
    /// reference client shows the number the caller asked for — so a caller
    /// that asked for five and was handed every level got a different book from
    /// the one it asked for.
    #[test]
    fn a_book_is_as_deep_as_the_caller_asked() {
        let mut farm = FarmState::new();
        let mut conn = None;
        let mut hb = HeartbeatState::new();
        let shared = SharedState::new();

        farm.send_depth_subscribe(1, 756733, "IEX", "", "STK", 5, false, &mut conn, &mut hb, &shared);
        assert!(farm.within_asked_depth(1, 0), "the top of the book");
        assert!(farm.within_asked_depth(1, 4), "the fifth level");
        assert!(!farm.within_asked_depth(1, 5), "and no deeper");

        // A caller that named no depth is not held to one.
        farm.send_depth_subscribe(2, 756733, "IEX", "", "STK", 0, false, &mut conn, &mut hb, &shared);
        assert!(farm.within_asked_depth(2, 99));

        // Withdrawn, and the depth goes with it rather than outliving the
        // request and applying to whatever reuses the number.
        farm.send_depth_unsubscribe(1, &mut conn, &mut hb);
        assert!(farm.within_asked_depth(1, 99));
    }

    /// A book is asked for once and withdrawn once, and what is withdrawn is
    /// what this client asked under rather than what the caller stated.
    #[test]
    fn withdrawing_a_book_withdraws_what_was_asked_for() {
        let mut farm = FarmState::new();
        let shared = SharedState::new();
        let mut conn = None;
        let mut hb = HeartbeatState::new();

        farm.send_depth_subscribe(7, 756733, "SMART", "", "STK", 10, true, &mut conn, &mut hb, &shared);
        assert_eq!(farm.depth_subs.len(), 1, "a book on no venue is one subscription");
        assert_eq!(farm.depth_fanout_map[0].1, 7, "and it is the caller's");
        assert_ne!(farm.depth_fanout_map[0].0, 7, "asked under an id of ours");

        farm.send_depth_unsubscribe(7, &mut conn, &mut hb);
        assert!(farm.depth_fanout_map.is_empty(), "nothing is left asking");
        assert!(farm.depth_subs.is_empty());
        assert!(farm.depth_fanout_exchange.is_empty());
        assert!(farm.depth_resub_info.is_empty(), "and no reconnect asks again");
    }

    /// A caller's number never carries two live wire subscriptions.
    ///
    /// The three records a row is routed by deduped on nothing, so a book
    /// asked for while this connection was down recorded a wire id nothing
    /// ever sent -- and when the reconnect asked properly and the venue
    /// refused, the refusal saw the phantom still asking and was swallowed:
    /// the caller waited for ever for a book that had been refused. The
    /// withdrawal then named the later contract only, leaving the earlier one
    /// served at the venue.
    #[test]
    fn a_book_asked_for_while_the_connection_is_down_leaves_no_second_record() {
        let mut farm = FarmState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();

        // Asked for with no socket: recorded so the reconnect can ask, and
        // nothing goes out.
        let mut down: Option<Connection> = None;
        farm.send_depth_subscribe(7, 756733, "SMART", "", "STK", 10, true, &mut down, &mut hb, &shared);
        let while_down = farm.depth_fanout_map.clone();

        // The reconnect asks again under the same number, as it must.
        let (conn, _peer) = Connection::for_test();
        let mut up = Some(conn);
        farm.send_depth_subscribe(7, 756733, "SMART", "", "STK", 10, true, &mut up, &mut hb, &shared);

        assert_eq!(
            farm.depth_fanout_map.iter().filter(|(_, user)| *user == 7).count(), 1,
            "one live subscription per caller: {:?} then {:?}",
            while_down, farm.depth_fanout_map,
        );
        assert_eq!(farm.depth_subs.len(), 1, "and one wire record for it");
        assert_eq!(farm.depth_fanout_exchange.len(), 1, "and one venue against it");
    }

    /// A reconnect tells every book's caller to empty it before what follows.
    ///
    /// The venue restarts the book from the top on the new connection, and
    /// every level of that restart is delivered as a level the caller does not
    /// already hold. Told nothing, a caller keyed on position refreshed the
    /// levels the new book reaches and kept every level the old one held below
    /// them, for the life of the session.
    #[test]
    fn a_rebuilt_connection_tells_a_book_to_start_again() {
        let mut farm = FarmState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();
        let mut context = Context::new();
        let (conn, _peer) = Connection::for_test();
        let mut up = Some(conn);

        farm.send_depth_subscribe(7, 756733, "SMART", "", "STK", 10, true, &mut up, &mut hb, &shared);
        let _ = shared.reference.drain_historical_errors();

        let (fresh, _peer2) = Connection::for_test();
        farm.reconnect(
            fresh, &mut up, &mut context, &mut hb, Default::default(), &shared,
        );

        let told = shared.reference.drain_historical_errors();
        assert!(
            told.iter().any(|(rid, code, _)| *rid == 7 && *code == 317),
            "the caller is told to empty its book: {told:?}",
        );
    }
}

mod depth_position_tests {
    use super::super::*;

    /// A withdrawal that names nothing on the wire still clears the caller's
    /// routing records. Returned early, the next contract asked for under
    /// that number inherited the old one's tag and read its book as its own.
    #[test]
    fn a_withdrawal_naming_nothing_on_the_wire_still_clears_the_routing() {
        let mut farm = FarmState::new();
        let mut hb = HeartbeatState::new();
        farm.depth_tag_to_req.push((0x11, 7, false, 0.01, 1.0, "IEX".to_string()));
        farm.depth_rows.push((7, 10));
        farm.send_depth_unsubscribe(7, &mut None, &mut hb);
        assert!(farm.depth_tag_to_req.is_empty(), "the tag record goes with the request");
        assert!(farm.depth_rows.is_empty(), "and so does the row count");
    }

    /// A refusal naming a number nobody holds is not published under it. The
    /// wire number of a book already withdrawn was handed back as if it were
    /// a caller's request number, and whoever held that number was told a
    /// book had been refused.
    #[test]
    fn a_refusal_naming_no_caller_is_not_published_under_the_wire_number() {
        let mut farm = FarmState::new();
        let shared = SharedState::new();
        let context = Context::new();
        let refused = crate::protocol::fix::fix_build(&[
            (crate::protocol::fix::TAG_MSG_TYPE, "j"),
            (262, "100"),
            (58, "Error&ISLAND/DEPTH/not available"),
        ], 1);
        farm.handle_subscription_reject(&refused, &context, &shared);
        assert!(
            shared.reference.drain_historical_errors().is_empty(),
            "nobody holds 100, so nobody is told",
        );
    }



    /// A refusal of a request riding beside the quote — the trading status,
    /// the exchange map, the option model — is the venue refusing that
    /// request, not the quote. Reported as the quote's, the caller was told
    /// no such contract exists (200) while its prices went on arriving, and a
    /// program that takes that number as final withdrew a working
    /// subscription.
    #[test]
    fn a_refused_companion_request_is_not_the_quote_refused() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut None, &mut hb,
        );
        let refused = |id: u32| crate::protocol::fix::fix_build(&[
            (crate::protocol::fix::TAG_MSG_TYPE, "j"),
            (262, &id.to_string()),
            (58, "Error&SMART/STATUS/not available"),
        ], 1);
        let companion = farm.generic_tick_reqs.iter()
            .find(|(_, kind)| *kind == TRADING_STATUS_REQUEST_TYPE)
            .map(|(id, _)| *id)
            .expect("the trading status is asked for beside the quote");
        farm.handle_subscription_reject(&refused(companion), &context, &shared);
        assert!(shared.market.drain_subscription_failures().is_empty(), "a companion's refusal is the companion's");
        // And it is told, not only logged. The venue names the request it is
        // refusing and says why — the one refusal channel on this wire that
        // does — and a caller that asked for the halt state or the option model
        // watched an acknowledged subscription that could never answer, with
        // the reason sitting in a log line. The kind is named, because "a
        // request beside the quote" is nothing a caller can act on.
        let told = shared.market.drain_companion_refusals();
        assert_eq!(told.len(), 1, "the companion's refusal reached nobody");
        assert_eq!(told[0].0, instrument);
        assert_eq!(told[0].1, TRADING_STATUS_REQUEST_TYPE);
        assert!(told[0].2.contains("whether the contract is halted"), "{:?}", told[0].2);
        assert!(told[0].2.contains("not available"), "the venue's own words: {:?}", told[0].2);
        let quote = farm.md_req_to_instrument.iter()
            .map(|(id, _)| *id)
            .find(|id| !farm.generic_tick_reqs.iter().any(|(g, _)| g == id))
            .expect("the quote's own request");
        farm.handle_subscription_reject(&refused(quote), &context, &shared);
        assert_eq!(shared.market.drain_subscription_failures().len(), 1, "the quote's own refusal reaches the caller");
        assert!(
            shared.market.drain_companion_refusals().is_empty(),
            "and the quote's own refusal is not a companion's",
        );
    }

    /// The quote a caller reads is zeroed at a drop, not only the engine's own.
    ///
    /// Zeroing the engine's copy is what stops a price from before the drop
    /// being read as current — but the copy the caller's tick poll reads is the
    /// shared one, and it was left standing. Against a baseline the drop had
    /// just cleared, every field of that stale quote read as a move and went out
    /// again as a fresh tick, under the notice saying the feed had gone.
    #[test]
    fn a_drop_zeroes_the_quote_the_caller_reads() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        shared.market.set_instrument_count(1);
        shared.market.push_quote(instrument, &crate::types::Quote {
            bid: 100 * crate::engine::hot_loop::PRICE_SCALE,
            ask: 101 * crate::engine::hot_loop::PRICE_SCALE,
            ..Default::default()
        });
        assert_ne!(shared.market.quote(instrument).bid, 0, "a price stands before the drop");

        farm.handle_disconnect(&mut None, &mut context, &None, &shared);

        let after = shared.market.quote(instrument);
        assert_eq!(after.bid, 0, "and none stands after it");
        assert_eq!(after.ask, 0);
    }

    /// The news that rides beside the quote is its own generic tick, and the
    /// venue refuses it on its own. Left in place, the entry the rebuild reads
    /// re-sends on the next reconnect a subscription the venue has already
    /// said it will not serve, and a headline that never comes is waited on
    /// forever. So the refusal releases it: the request the venue named, the
    /// tag it was filed under, and the entry the rebuild walks — while the
    /// quote it rode beside is not reported refused and its prices go on
    /// arriving.
    #[test]
    fn a_refused_news_companion_releases_its_state() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut None, &mut hb,
        );
        farm.send_news_subscribe(756733, instrument, "STK", "BRFG", 7, &mut None, &mut hb);
        assert_eq!(farm.news_subscriptions.len(), 1, "the news was filed for the rebuild");
        let refused = crate::protocol::fix::fix_build(&[
            (crate::protocol::fix::TAG_MSG_TYPE, "j"),
            (262, "7"),
            (58, "Error&BRFG/NEWS/not permissioned"),
        ], 1);
        farm.handle_subscription_reject(&refused, &context, &shared);
        assert!(farm.news_subscriptions.is_empty(), "the refused news is not left for the rebuild to re-send");
        assert!(farm.generic_tick_reqs.iter().all(|(rid, _)| *rid != 7), "its request is released");
        assert!(farm.md_req_to_instrument.iter().all(|(rid, _)| *rid != 7), "and its instrument mapping");
        assert!(shared.market.drain_subscription_failures().is_empty(), "the quote it rode beside is not reported refused");
        assert_eq!(shared.market.drain_news_rejections(), vec![756733], "the client is told to clear its askers so a re-ask sends");
    }

    /// The increment the venue acknowledges a subscription with is kept for
    /// the caller, who hears it on `tick_req_params` as the reference client
    /// delivers it.
    #[test]
    fn an_acknowledged_increment_is_kept_for_the_caller() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut None, &mut hb,
        );
        let quote = farm.md_req_to_instrument.iter()
            .map(|(id, _)| *id)
            .find(|id| !farm.generic_tick_reqs.iter().any(|(g, _)| g == id))
            .expect("the quote's own request");
        let ack = format!("35=Q\x01777,{quote},0.01");
        farm.handle_subscription_ack(ack.as_bytes(), &mut context, &shared);
        assert_eq!(shared.market.drain_tick_req_params(), vec![(instrument, 0.01)]);
    }

}

mod exchange_map_tests {
    use super::super::*;
    use crate::bridge::SharedState;
    use crate::engine::context::Context;

    /// The exchange map payload states its length before its text and pads to
    /// a four-byte boundary after it. Read end to end as text, the length
    /// bytes join the first name and the padding joins the last, so the mask's
    /// bits are reported against names that do not exist.
    ///
    /// Each entry names the bit it answers to, then the single letter it is
    /// shown as, then its name. Read as a name and a letter, with the bit
    /// taken from where the entry sat, a bid on two venues rendered
    /// `J/EDGEAY/BYX` — every letter run together with its own name — and a
    /// list that numbers its own bits was renumbered by position.
    #[test]
    fn the_exchange_map_reads_the_text_its_payload_states() {
        let text = b"9/J/EDGEA;10/Y/BYX;12/P/ARCA";
        let mut payload = Vec::new();
        payload.extend_from_slice(&(text.len() as u32).to_be_bytes());
        payload.extend_from_slice(text);
        // Alignment: bytes after the text are discarded until the count is a
        // multiple of four.
        while (payload.len() - 4) % 4 != 0 {
            payload.push(0);
        }

        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_be_bytes());
        body.push(payload.len() as u8);
        body.extend_from_slice(&payload);
        let mut msg = b"35=G\x01".to_vec();
        msg.extend_from_slice(&(((body.len() * 8) % 65_536) as u16).to_be_bytes());
        msg.extend_from_slice(&body);

        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        farm.generic_tick_tags.push((7, BBO_EXCHANGE_MAP_REQUEST_TYPE, instrument));
        farm.handle_generic_tick(&msg, &mut context, &shared, &None);

        let named = shared.reference.smart_components();
        assert_eq!(named.len(), 3, "every venue the map names: {named:?}");
        assert_eq!(named[0].bit_number, 9, "the bit the entry states, not where it sat");
        assert_eq!(named[0].exchange, "EDGEA", "and its own name: {named:?}");
        assert_eq!(named[0].exchange_letter, "J", "the letter alone");
        assert_eq!(named[2].bit_number, 12, "and the last states its own too: {named:?}");
        assert_eq!(named[2].exchange, "ARCA");
        assert_eq!(named[2].exchange_letter, "P");

        // Rendered against a mask, the letters are letters.
        assert_eq!(
            crate::client_core::render_exchange_mask((1 << 9) | (1 << 10), &shared),
            "JY",
            "two venues, two letters",
        );
    }

    /// An entry that does not name a bit, a letter and a venue is not an
    /// entry. Read as one, whatever it does carry stands in for a letter.
    #[test]
    fn an_entry_short_of_its_three_parts_names_no_venue() {
        let text = b"NYSE/N;NASDAQ/Q";
        let mut payload = Vec::new();
        payload.extend_from_slice(&(text.len() as u32).to_be_bytes());
        payload.extend_from_slice(text);
        while (payload.len() - 4) % 4 != 0 {
            payload.push(0);
        }
        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_be_bytes());
        body.push(payload.len() as u8);
        body.extend_from_slice(&payload);
        let mut msg = b"35=G\x01".to_vec();
        msg.extend_from_slice(&(((body.len() * 8) % 65_536) as u16).to_be_bytes());
        msg.extend_from_slice(&body);

        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        farm.generic_tick_tags.push((7, BBO_EXCHANGE_MAP_REQUEST_TYPE, instrument));
        farm.handle_generic_tick(&msg, &mut context, &shared, &None);

        assert!(
            shared.reference.smart_components().is_empty(),
            "nothing the mask's bits could be read against",
        );
    }
}

/// An acknowledgement stating an increment nothing can be counted in is
/// refused, and whoever asked is told.
///
/// Every price and every size on the instrument is a count of the increment,
/// and this parser reads `inf` from the word. Taken as stated, an infinite one
/// scales every price on the contract to the end of the range and every level
/// of the book with it; a zero or a negative one erases or inverts them. The
/// prices then stop reaching the caller with nothing said, which reads as a
/// contract nobody is quoting.
#[test]
fn an_increment_prices_cannot_be_counted_in_is_refused_and_reported() {
    use crate::bridge::SharedState;
    use crate::engine::context::Context;

    for stated in ["inf", "-0.01", "0", "NaN"] {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let shared = SharedState::new();
        let instrument = context.market.register(756733);
        farm.md_req_to_instrument.push((7, instrument));

        let msg = format!("35=Q\x0133082,7,{stated},0,3");
        farm.handle_subscription_ack(msg.as_bytes(), &mut context, &shared);

        assert_eq!(
            context.market.min_tick(instrument), 0.0,
            "{stated} is not an increment, so the instrument is left with none",
        );
        let told = shared.market.drain_subscription_failures();
        assert!(
            told.iter().any(|(id, why)| *id == instrument && why.contains(stated)),
            "and the caller is told which one was refused: {told:?}",
        );
    }

    // The size increment rides the same acknowledgement and is read the same
    // way: an infinite one counts every size on the contract to the end of the
    // range.
    assert_eq!(
        trailing_size_increment(&["33082", "6", "0.01", "", "inf"]), None,
        "an infinite size increment is no increment either",
    );
}

/// A size increment is read only from an acknowledgement shaped like one that
/// carries it.
///
/// Captured off the wire, a ticker setup is five fields and a subscription
/// acknowledgement is nine, both ending on the increment. Every neighbouring
/// field parses as a positive number, so a shorter acknowledgement would not
/// fail the parse — it would hand back the field before it, and a server tag
/// read as a size increment multiplies every size on the contract by five
/// figures.
#[test]
fn a_size_increment_comes_only_from_an_ack_shaped_to_carry_one() {
    // The two shapes, as the venue sent them.
    let setup = ["893091670", "0.25", "33079", "", "1"];
    assert_eq!(trailing_size_increment(&setup), Some(1.0));
    let subscribed = ["33082", "6", "0.01", "0", "3", "a6", "", "1", "0.5"];
    assert_eq!(trailing_size_increment(&subscribed), Some(0.5));

    // And one too short to carry it, whose last field is a server tag.
    let short = ["893091670", "0.25", "33079"];
    assert_eq!(
        trailing_size_increment(&short), None,
        "a server tag is not a size increment",
    );
}

mod withdrawal_wire_tests {
    use super::super::*;
    use crate::bridge::SharedState;
    use crate::engine::context::Context;
    use std::collections::BTreeSet;

    /// Every value a message carries for one tag, in order. Split on the
    /// field mark, so a tag stated more than once states each of its values,
    /// which a map keyed by the tag cannot hold.
    fn values_of(msg: &[u8], tag: u32) -> Vec<String> {
        let prefix = format!("{tag}=");
        msg.split(|&b| b == 0x01)
            .filter_map(|field| {
                let field = std::str::from_utf8(field).ok()?;
                field.strip_prefix(prefix.as_str()).map(|v| v.to_string())
            })
            .collect()
    }

    /// A withdrawal states each entry the way the subscription stated it.
    ///
    /// Named by the number alone, the venue leaves the subscription being
    /// served. The number then stays held on the connection, and an engine
    /// that starts on the same connection and asks under it is answered with
    /// nothing — no acknowledgement, no refusal, no data — while the quotes
    /// it never asked for keep arriving under the number it happens to have
    /// given out.
    #[test]
    fn a_withdrawal_states_the_entries_the_subscription_stated() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let mut hb = HeartbeatState::new();
        let instrument = context.market.register(756733);

        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", instrument, 0,
            false, &mut conn, &mut hb,
        );
        let mut asked = BTreeSet::new();
        for msg in super::drain_inner(&mut peer) {
            if values_of(&msg, 263).first().map(String::as_str) == Some("1") {
                asked.extend(values_of(&msg, 262));
            }
        }
        assert_eq!(asked.len(), 4, "a realtime stock is asked for under four numbers");

        farm.send_mktdata_unsubscribe(instrument, 0, 0, &[], u64::MAX, false, &mut conn, &mut hb);
        let withdrawals: Vec<Vec<u8>> = super::drain_inner(&mut peer)
            .into_iter()
            .filter(|msg| values_of(msg, 263).first().map(String::as_str) == Some("2"))
            .collect();
        let withdrawn: BTreeSet<String> = withdrawals
            .iter()
            .flat_map(|msg| values_of(msg, 262))
            .collect();
        assert_eq!(withdrawn, asked, "every number asked for is withdrawn");
        for msg in &withdrawals {
            assert_eq!(
                values_of(msg, 146).first().map(String::as_str), Some("1"),
                "one entry per withdrawal",
            );
            assert_eq!(
                values_of(msg, 6008).first().map(String::as_str), Some("756733"),
                "the contract is stated",
            );
            assert_eq!(
                values_of(msg, 207).first().map(String::as_str), Some("BEST"),
                "the venue it was asked on",
            );
            assert_eq!(
                values_of(msg, 167).first().map(String::as_str), Some("CS"),
                "the type it was asked for",
            );
            assert!(
                !values_of(msg, 264).is_empty(),
                "the kind of market data it asked for",
            );
            // And the rest of the fields the subscription carried. The venue
            // writes one entry whichever action carries it, so an entry short
            // of them is not the entry that went out coming back.
            for (tag, stated) in [(6088, "Socket"), (9830, "1"), (9839, "1")] {
                assert_eq!(
                    values_of(msg, tag).first().map(String::as_str), Some(stated),
                    "tag {tag} is stated the way the subscription stated it",
                );
            }
        }
    }

    /// A book is withdrawn the way it was asked for. Left named by its
    /// number alone, the venue keeps serving it, and the number stays held
    /// against the next engine on the connection.
    #[test]
    fn withdrawing_a_book_states_the_book_it_withdraws() {
        let mut farm = FarmState::new();
        let shared = SharedState::new();
        let mut hb = HeartbeatState::new();

        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).expect("a connection over the test pair");

        farm.send_depth_subscribe(
            7, 756733, "SMART", "", "STK", 10, true, &mut conn, &mut hb, &shared,
        );
        let asked = super::drain_inner(&mut peer);
        let book = asked.iter()
            .find(|msg| values_of(msg, 264).first().map(String::as_str) == Some("0"))
            .expect("the book went out");
        assert_eq!(
            values_of(book, 9839).first().map(String::as_str), Some("1"),
            "a book states 9839 the way every other entry does",
        );

        farm.send_depth_unsubscribe(7, &mut conn, &mut hb);
        let withdrawals: Vec<Vec<u8>> = super::drain_inner(&mut peer)
            .into_iter()
            .filter(|msg| values_of(msg, 263).first().map(String::as_str) == Some("2"))
            .collect();
        assert_eq!(
            withdrawals.len(), 1,
            "a book on no particular venue is one withdrawal",
        );
        let msg = &withdrawals[0];
        assert_eq!(values_of(msg, 146).first().map(String::as_str), Some("1"), "of one entry");
        assert_eq!(
            values_of(msg, 6008).first().map(String::as_str), Some("756733"),
            "naming the contract",
        );
        assert_eq!(
            values_of(msg, 207).first().map(String::as_str), Some("BEST"),
            "the venue it was asked on",
        );
        assert_eq!(
            values_of(msg, 167).first().map(String::as_str), Some("CS"),
            "the type it was asked for",
        );
        assert_eq!(values_of(msg, 264).first().map(String::as_str), Some("0"), "and a book");
        for (tag, stated) in [(6088, "Socket"), (9830, "1"), (9839, "1")] {
            assert_eq!(
                values_of(msg, tag).first().map(String::as_str), Some(stated),
                "tag {tag} is stated the way the subscription stated it",
            );
        }
    }
}

mod depth_bit_tests {
    use super::super::*;
    use super::decode_publish_tests::push_bits;

    /// One field as the wire carries it: the id (its meaning is `id >> 2`),
    /// the value's width in bytes, and the value, signed.
    struct Field { id: u64, len: usize, value: i64 }
    /// One entry: the operation, the maker's name, the position and the fields.
    struct Entry<'a> { op: u64, name: &'a str, position: u64, fields: Vec<Field> }

    const BID_PX: u64 = 0;
    const ASK_PX: u64 = 4;
    const BID_SZ: u64 = 16;
    const ASK_SZ: u64 = 20;

    fn level(op: u64, name: &str, position: u64, px_id: u64, px: i64, sz_id: u64, sz: i64) -> Entry<'_> {
        Entry { op, name, position, fields: vec![
            Field { id: px_id, len: 2, value: px },
            Field { id: sz_id, len: 2, value: sz },
        ] }
    }

    /// One 35=Y frame, written the way the wire lays it out: a two-byte bit
    /// count, then sections of entries of fields, each list closed by the
    /// flag on its last item.
    fn framed_35y(sections: &[(u32, Vec<Entry<'_>>)]) -> Vec<u8> {
        let mut bits: Vec<u8> = Vec::new();
        for (si, (tag, entries)) in sections.iter().enumerate() {
            push_bits(&mut bits, u64::from(si + 1 < sections.len()), 1);
            push_bits(&mut bits, u64::from(*tag), 31);
            for (ei, e) in entries.iter().enumerate() {
                push_bits(&mut bits, u64::from(ei + 1 < entries.len()), 1);
                push_bits(&mut bits, 0, 1);
                push_bits(&mut bits, e.op, 2);
                push_bits(&mut bits, e.name.len() as u64, 4);
                for b in e.name.bytes() { push_bits(&mut bits, u64::from(b), 8); }
                push_bits(&mut bits, e.position, 8);
                for (fi, f) in e.fields.iter().enumerate() {
                    let more = u64::from(fi + 1 < e.fields.len());
                    if f.id >= 31 || f.len > 4 {
                        push_bits(&mut bits, 31, 5);
                        push_bits(&mut bits, more, 1);
                        push_bits(&mut bits, 0, 2);
                        push_bits(&mut bits, f.id, 8);
                        push_bits(&mut bits, f.len as u64, 8);
                    } else {
                        push_bits(&mut bits, f.id, 5);
                        push_bits(&mut bits, more, 1);
                        push_bits(&mut bits, (f.len - 1) as u64, 2);
                    }
                    push_bits(&mut bits, u64::from(f.value < 0), 1);
                    // A magnitude wider than a machine word: the high bits are
                    // written as zeros, then the word. A field stating a width
                    // of nothing carries the sign bit and no magnitude, which
                    // is how the wire writes one.
                    let width = (8 * f.len).saturating_sub(1);
                    if width > 64 {
                        push_bits(&mut bits, 0, width - 64);
                        push_bits(&mut bits, f.value.unsigned_abs(), 64);
                    } else {
                        push_bits(&mut bits, f.value.unsigned_abs(), width);
                    }
                }
            }
        }
        let mut payload = vec![0u8; bits.len().div_ceil(8)];
        for (i, &b) in bits.iter().enumerate() {
            if b == 1 { payload[i >> 3] |= 1 << (7 - (i & 7)); }
        }
        let mut msg = b"35=Y\x01".to_vec();
        msg.push((bits.len() >> 8) as u8);
        msg.push((bits.len() & 0xFF) as u8);
        msg.extend_from_slice(&payload);
        msg
    }

    fn farm_holding(tag: u32, req_id: u32, venue: &str) -> (FarmState, SharedState) {
        let mut farm = FarmState::new();
        farm.depth_tag_to_req.push((tag, req_id, false, 0.01, 1.0, venue.to_string()));
        (farm, SharedState::new())
    }

    /// A level arrives with the operation and the side the wire states, and
    /// the maker's name as the wire states it.
    ///
    /// On an exchange-level book that is the name and nothing else: a book
    /// quoting no makers states none, and a caller keying its book by maker
    /// was handed a maker named after the exchange that the venue never named.
    /// The aggregated book is the one place the exchange stands in, which is
    /// where the aggregating reader puts it.
    #[test]
    fn a_level_carries_its_operation_its_side_and_its_maker() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        farm.handle_depth_35y(&framed_35y(&[(0x1122, vec![
            level(0, "NSDQ", 0, BID_PX, 10050, BID_SZ, 500),
            level(1, "", 1, ASK_PX, 10075, ASK_SZ, 300),
        ])]), &shared);
        let got: Vec<_> = shared.market.drain_depth_updates().into_iter()
            .map(|u| (u.operation, u.side, u.position, u.price, u.size, u.market_maker))
            .collect();
        assert_eq!(got, [
            (0, 1, 0, 100.50, 500.0, "NSDQ".to_string()),
            (1, 0, 1, 100.75, 300.0, String::new()),
        ], "{got:?}");

        // And on the aggregated book, where the venue states no maker, the
        // exchange the section was asked on stands in.
        let mut smart = FarmState::new();
        smart.depth_tag_to_req.push((0x1122, 9, true, 0.01, 1.0, "IEX".to_string()));
        let aggregated = SharedState::new();
        smart.handle_depth_35y(&framed_35y(&[(0x1122, vec![
            level(0, "", 0, BID_PX, 10050, BID_SZ, 500),
        ])]), &aggregated);
        let got: Vec<String> = aggregated.market.drain_depth_updates().into_iter()
            .map(|u| u.market_maker)
            .collect();
        assert_eq!(got, ["IEX".to_string()], "{got:?}");
    }

    /// A delete is an operation of its own, naming its side and no level.
    /// Read by byte shape, its entry byte was neither shape looked for and
    /// was skipped, so a book here could never shrink.
    #[test]
    fn a_delete_is_delivered_as_one_and_carries_no_level() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        farm.handle_depth_35y(&framed_35y(&[(0x1122, vec![
            Entry { op: 2, name: "", position: 3, fields: vec![] },
            Entry { op: 3, name: "NSDQ", position: 0, fields: vec![] },
            level(0, "", 4, BID_PX, 9900, BID_SZ, 10),
        ])]), &shared);
        let got: Vec<_> = shared.market.drain_depth_updates().into_iter()
            .map(|u| (u.operation, u.side, u.position, u.price, u.size))
            .collect();
        assert_eq!(got, [
            (2, 1, 3, 0.0, 0.0),
            (2, 0, 0, 0.0, 0.0),
            (0, 1, 4, 99.0, 10.0),
        ], "{got:?}");
    }

    /// An entry at the top of the book with no maker named is an entry. Read
    /// by byte shape it was a section switch, and it and every level after it
    /// in the frame were lost.
    #[test]
    fn an_unnamed_entry_at_the_top_of_the_book_is_an_entry() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        farm.handle_depth_35y(&framed_35y(&[(0x1122, vec![
            level(0, "", 0, BID_PX, 10000, BID_SZ, 100),
            level(0, "", 0, ASK_PX, 10001, ASK_SZ, 200),
        ])]), &shared);
        let got: Vec<_> = shared.market.drain_depth_updates().into_iter()
            .map(|u| (u.side, u.position, u.price)).collect();
        assert_eq!(got, [(1, 0, 100.0), (0, 0, 100.01)], "{got:?}");
    }

    /// A frame can switch into a stream this session does not hold: the
    /// withdrawal of a book is on its way to the venue while the frames it
    /// already sent are still arriving. Each section names the stream its
    /// levels belong to, and nothing from a section this session does not
    /// hold is delivered.
    #[test]
    fn a_section_for_a_stream_this_session_does_not_hold_delivers_nothing() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        farm.handle_depth_35y(&framed_35y(&[
            (0x1122, vec![level(0, "TEST", 0, BID_PX, 100, BID_SZ, 5)]),
            (0x0005, vec![level(0, "", 0, BID_PX, 100, BID_SZ, 10)]),
            (0x1122, vec![level(0, "", 1, BID_PX, 200, BID_SZ, 7)]),
        ]), &shared);
        let got: Vec<_> = shared.market.drain_depth_updates().into_iter()
            .map(|u| (u.position, u.price, u.size, u.market_maker)).collect();
        assert_eq!(got, [
            (0, 1.0, 5.0, "TEST".to_string()),
            (1, 2.0, 7.0, String::new()),
        ], "only the held stream's levels: {got:?}");
    }

    /// A field this client does not read is read past, whatever its width,
    /// and a value's sign is its own bit.
    #[test]
    fn a_field_not_read_is_stepped_over_and_a_sign_is_honoured() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        farm.handle_depth_35y(&framed_35y(&[(0x1122, vec![Entry { op: 0, name: "", position: 2, fields: vec![
            Field { id: 100, len: 3, value: -5 },
            Field { id: 8, len: 1, value: 3 },
            Field { id: BID_PX, len: 4, value: -1_234 },
            Field { id: BID_SZ, len: 1, value: 42 },
        ] }])]), &shared);
        let got: Vec<_> = shared.market.drain_depth_updates().into_iter()
            .map(|u| (u.position, u.price, u.size)).collect();
        assert_eq!(got, [(2, -12.34, 42.0)], "{got:?}");
    }

    /// A field wider than a number this reads is stepped over, and the
    /// level after it is still delivered. Ended at that field, every entry
    /// after it in the frame was lost, silently from the caller's side.
    #[test]
    fn a_field_wider_than_a_number_is_stepped_over() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        farm.handle_depth_35y(&framed_35y(&[(0x1122, vec![
            Entry { op: 0, name: "", position: 2, fields: vec![
                Field { id: 100, len: 12, value: 5 },
                Field { id: BID_PX, len: 2, value: 10050 },
                Field { id: BID_SZ, len: 1, value: 7 },
            ] },
            level(0, "", 3, ASK_PX, 10075, ASK_SZ, 9),
        ])]), &shared);
        let got: Vec<_> = shared.market.drain_depth_updates().into_iter()
            .map(|u| (u.position, u.price, u.size)).collect();
        assert_eq!(got, [(2, 100.50, 7.0), (3, 100.75, 9.0)], "{got:?}");
    }

    /// A caller asking for five levels keeps five: the place a withdrawal
    /// empties is filled from the level that moved up into it.
    ///
    /// The venue sends the whole book and says nothing about the level that
    /// moves up when one near the top goes away: it arrives one place below
    /// what the caller asked for and is dropped as too deep. With no book of
    /// its own this client had nothing to put in the place that emptied, so
    /// the caller's book lost a row on every withdrawal near the top and never
    /// refilled — and the row at the bottom of the window was stale from then
    /// on. Both compensations are the ones the client this replaces makes.
    #[test]
    fn the_place_a_withdrawal_empties_is_filled_from_the_book() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        farm.depth_rows.push((7, 3));

        // Four levels on the bid: three inside the window, one below it.
        farm.handle_depth_35y(&framed_35y(&[(0x1122, vec![
            level(0, "A", 0, BID_PX, 10050, BID_SZ, 100),
            level(0, "B", 1, BID_PX, 10040, BID_SZ, 200),
            level(0, "C", 2, BID_PX, 10030, BID_SZ, 300),
            level(0, "D", 3, BID_PX, 10020, BID_SZ, 400),
        ])]), &shared);
        let inside: Vec<i32> = shared.market.drain_depth_updates()
            .into_iter().map(|u| u.position).collect();
        assert_eq!(inside, [0, 1, 2], "the window the caller asked for: {inside:?}");

        // The top of the book goes away. The level that was below the window
        // moves into its last place, and that is what the caller is owed.
        farm.handle_depth_35y(&framed_35y(&[(0x1122, vec![
            Entry { op: 2, name: "", position: 0, fields: vec![] },
        ])]), &shared);
        let after: Vec<(i32, i32, String, f64)> = shared.market.drain_depth_updates()
            .into_iter()
            .map(|u| (u.operation, u.position, u.market_maker, u.price))
            .collect();
        assert_eq!(
            after,
            [
                (2, 0, String::new(), 0.0),
                (0, 2, "D".to_string(), 100.20),
            ],
            "the withdrawal, then the level that moved up into the place it left: {after:?}",
        );

        // And a level arriving inside the window pushes one out of it, which
        // the venue also says nothing about.
        farm.handle_depth_35y(&framed_35y(&[(0x1122, vec![
            level(0, "E", 0, BID_PX, 10060, BID_SZ, 500),
        ])]), &shared);
        let pushed: Vec<(i32, i32, String)> = shared.market.drain_depth_updates()
            .into_iter()
            .map(|u| (u.operation, u.position, u.market_maker))
            .collect();
        assert_eq!(
            pushed,
            [
                // Named as the row it withdraws was named, so it reaches the
                // caller on the same callback as every row beside it.
                (2, 2, "D".to_string()),
                (0, 0, "E".to_string()),
            ],
            "the level pushed out of the window goes first: {pushed:?}",
        );
    }

    /// A book past the wrap of its own bit count is read whole.
    ///
    /// Two bytes state the count, so it repeats every sixty-five thousand five
    /// hundred and thirty-six. A book of four thousand withdrawals is a full
    /// cycle exactly and states nought: read as stated, every withdrawal in it
    /// was dropped and the caller's book kept levels the venue had just taken
    /// away.
    #[test]
    fn a_book_past_the_wrap_of_its_own_bit_count_is_read_whole() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        // A withdrawal carries no fields and no name, so each is sixteen
        // bits; four thousand and ninety four of them behind a thirty-two bit
        // section header is the cycle exactly.
        let mut entries: Vec<Entry<'_>> = (0..4_093)
            .map(|i| Entry { op: 2, name: "", position: (i % 256) as u64, fields: vec![] })
            .collect();
        // The last one is the reading: it arrives only if the count was
        // recovered.
        entries.push(Entry { op: 0, name: "", position: 9, fields: vec![
            Field { id: BID_PX, len: 2, value: 10123 },
            Field { id: BID_SZ, len: 1, value: 4 },
        ] });
        let msg = framed_35y(&[(0x1122, entries)]);
        assert_eq!(
            u16::from_be_bytes([msg[b"35=Y\x01".len()], msg[b"35=Y\x01".len() + 1]]), 40,
            "the stated count has wrapped, which is what this exercises",
        );
        farm.handle_depth_35y(&msg, &shared);
        let got = shared.market.drain_depth_updates();
        assert!(
            got.iter().any(|u| u.position == 9 && (u.price - 101.23).abs() < 1e-9),
            "the level past the wrap arrived: {} updates", got.len(),
        );
    }

    /// A field stating a width of nothing still costs its sign bit.
    ///
    /// Every field carries one, whatever the width says. Skipped by the width
    /// alone, the walk was left one bit behind the wire and every field and
    /// entry after it in the frame was read from shifted bits — and handed to
    /// the caller as a priced, sized level.
    #[test]
    fn a_field_stating_no_width_still_costs_its_sign_bit() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        farm.handle_depth_35y(&framed_35y(&[(0x1122, vec![
            Entry { op: 0, name: "", position: 4, fields: vec![
                // The extended form, stating a width of nothing: read at all,
                // it is the sign bit and no magnitude.
                Field { id: 100, len: 0, value: 0 },
                Field { id: BID_PX, len: 2, value: 10050 },
                Field { id: BID_SZ, len: 1, value: 5 },
            ] },
        ])]), &shared);
        let got: Vec<_> = shared.market.drain_depth_updates().into_iter()
            .map(|u| (u.position, u.price, u.size)).collect();
        assert_eq!(
            got, [(4, 100.50, 5.0)],
            "the fields behind it are read from the bits they were written on: {got:?}",
        );
    }

    /// The frame ends where its bit count says, not where the bytes do.
    #[test]
    fn the_frame_ends_at_its_stated_bit_count() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        let mut msg = framed_35y(&[(0x1122, vec![level(0, "", 0, BID_PX, 100, BID_SZ, 5)])]);
        // Another whole level's worth of bytes after the count: not read.
        let trailing = framed_35y(&[(0x1122, vec![level(0, "", 1, BID_PX, 300, BID_SZ, 9)])]);
        msg.extend_from_slice(&trailing[b"35=Y\x01".len() + 2..]);
        farm.handle_depth_35y(&msg, &shared);
        assert_eq!(shared.market.drain_depth_updates().len(), 1, "one level, as counted");
    }

    /// A level is a price and a size. An entry stating one alone would report
    /// the other as zero, which is not a quoted level, so it is left out and
    /// the entries after it are read as before.
    #[test]
    fn a_half_stated_level_is_not_a_level() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        farm.handle_depth_35y(&framed_35y(&[(0x1122, vec![
            Entry { op: 0, name: "", position: 0, fields: vec![Field { id: BID_PX, len: 2, value: 10000 }] },
            Entry { op: 1, name: "", position: 1, fields: vec![Field { id: ASK_SZ, len: 1, value: 9 }] },
            level(0, "", 2, BID_PX, 9999, BID_SZ, 3),
        ])]), &shared);
        let got: Vec<_> = shared.market.drain_depth_updates().into_iter().map(|u| u.position).collect();
        assert_eq!(got, [2], "the whole level, and neither half: {got:?}");
    }

    /// A level deeper than the rows asked for is not delivered.
    #[test]
    fn a_level_beyond_the_rows_asked_for_is_not_delivered() {
        let (mut farm, shared) = farm_holding(0x1122, 7, "IEX");
        farm.depth_rows.push((7, 2));
        farm.handle_depth_35y(&framed_35y(&[(0x1122, vec![
            level(0, "", 1, BID_PX, 100, BID_SZ, 5),
            level(0, "", 5, BID_PX, 100, BID_SZ, 5),
        ])]), &shared);
        let got: Vec<_> = shared.market.drain_depth_updates().into_iter().map(|u| u.position).collect();
        assert_eq!(got, [1], "{got:?}");
    }

/// A subscription opened after another was withdrawn is answered, whatever
/// number the venue puts on it — end to end, on the path that carries prices.
///
/// The venue hands its numbers out again. Every withdrawn one was kept in a set
/// and any answer naming one was refused, so a subscription that drew a
/// recycled number was acknowledged, bound to nothing and left silent: no
/// quotes, no refusal, for the rest of the session. Measured on a live feed, a
/// caller that subscribed, withdrew and subscribed again received 36 ticks and
/// then nothing at all, twice over, with no error raised.
///
/// The neighbours of this check cover the mapping and the ticker setup. This
/// one drives the acknowledgement that carries prices, which is where the feed
/// actually died.
#[test]
fn a_subscription_after_a_withdrawal_is_answered_under_a_recycled_number() {
    let mut farm = FarmState::new();
    let mut context = Context::new();
    let shared = SharedState::new();
    let mut hb = HeartbeatState::new();

    // One contract, subscribed and acknowledged under the venue's number.
    let first = context.market.register(265_598);
    farm.send_mktdata_subscribe(
        265_598, "AAPL", "SMART", "STK", "", 0.0, "", "", first, 0,
        false, &mut None, &mut hb,
    );
    let asked_under = farm.md_req_to_instrument[0].0;
    farm.handle_subscription_ack(
        format!("35=Q\x014242,{asked_under},0.01,0,3").as_bytes(), &mut context, &shared,
    );
    assert_eq!(
        context.market.instrument_by_server_tag(4242), Some(first),
        "the first subscription is bound to the number it was given",
    );

    // Withdrawn, and the slot given up — which is what frees the number at the
    // venue for the next caller.
    farm.send_mktdata_unsubscribe(first, 0, 0, &[], u64::MAX, false, &mut None, &mut hb);
    context.market.unregister(first);

    // The next contract, and the venue answers it under the number the last one
    // held.
    let next = context.market.register(756_733);
    farm.send_mktdata_subscribe(
        756_733, "SPY", "SMART", "STK", "", 0.0, "", "", next, 0,
        false, &mut None, &mut hb,
    );
    let asked_again = farm.md_req_to_instrument.last().expect("a request is waiting").0;
    farm.handle_subscription_ack(
        format!("35=Q\x014242,{asked_again},0.01,0,3").as_bytes(), &mut context, &shared,
    );

    assert_eq!(
        context.market.instrument_by_server_tag(4242), Some(next),
        "the subscription that holds the number now is the one it routes to; \
         refused for having been given up, it received nothing and was told nothing",
    );
}

/// A number the venue hands out again is taken for the contract it now names.
///
/// A set of numbers this session had withdrawn was held, and an answer naming
/// any of them was refused wherever it arrived. The venue reuses its numbers:
/// a subscription opened after another was withdrawn is routinely answered
/// under the number the withdrawn one held. Refused, it was acknowledged,
/// bound to nothing and left silent — no quotes, and no refusal to say so —
/// on any contract whose number came round again, for the rest of the session.
#[test]
fn a_number_the_venue_hands_out_again_is_taken_for_what_it_now_names() {
    let mut farm = FarmState::new();
    let mut context = Context::new();
    let shared = SharedState::new();

    // A subscription that ends, and the number it held.
    let gone = context.market.register(265598);
    context.market.register_server_tag(4242, gone);
    context.market.clear_server_tags_for(gone);

    // The next subscription, which the venue answers under the same number.
    let now = context.market.register(756733);
    farm.handle_ticker_setup(b"35=L\x01756733,0.01,4242", &mut context, &shared);

    assert_eq!(
        context.market.instrument_by_server_tag(4242), Some(now),
        "the answer names the contract that holds the number now",
    );
}

    /// A refused snapshot is not a refusal of the contract.
    ///
    /// The acknowledgement path already says why: the chargeable snapshot is a
    /// request of its own, nothing joins it, and what the venue says about it
    /// says nothing about the stream on the same contract. The refusal side
    /// had no such reading, so a snapshot declined for want of the entitlement
    /// — the documented outcome — was recorded against the contract. Every
    /// caller watching a healthy, ticking stream was told their quote had been
    /// refused, and nothing cleared it: only a fresh acknowledgement does, and
    /// a subscribe that joins an existing subscription never draws one.
    #[test]
    fn a_refused_snapshot_does_not_refuse_the_stream_beside_it() {
        let mut farm = FarmState::new();
        let mut context = Context::new();
        let instrument = context.register_instrument(756733);
        let shared = SharedState::new();
        // A live stream, and a snapshot out on the same contract.
        farm.instrument_md_reqs.push((instrument, MdReqRecord {
            con_id: 756733,
            sec_type: "CS".into(),
            mode_9887: 0,
            entries: vec![
                MdReqEntry { req_id: 7, request_type: 442, venue: "BEST".into() },
                MdReqEntry {
                    req_id: 8,
                    request_type: REGULATORY_SNAPSHOT_REQUEST_TYPE,
                    venue: "BEST".into(),
                },
            ],
        }));
        farm.md_req_to_instrument.push((8, instrument));

        let refused = fix::fix_build(
            &[(35, "3"), (262, "8"), (58, "Error&BEST/NO_ENTITLEMENT/snapshot")], 1,
        );
        farm.handle_subscription_reject(&refused, &context, &shared);

        assert!(
            shared.market.failure_for_follower(instrument).is_none(),
            "the stream on the contract was reported to its watchers as refused",
        );
    }

    /// A book the venue will not serve is not asked for again every reconnect.
    ///
    /// The withdrawal drops the record a reconnect rebuilds from, and says why:
    /// left behind, a book the caller let go was asked for again by the next
    /// reconnect. A refusal ends the book just as finally and dropped only the
    /// routing, so every reconnect told the caller its book had been emptied,
    /// re-sent the book, and drew the same refusal — two messages a reconnect
    /// for the rest of the session, on a request already answered once. The
    /// headlines beside it release their own replay record for this reason.
    #[test]
    fn a_refused_book_is_not_asked_for_again_by_the_next_reconnect() {
        let mut farm = FarmState::new();
        let context = Context::new();
        let shared = SharedState::new();
        // A book asked for under one wire number on the caller's behalf.
        farm.depth_fanout_map.push((900, 7));
        farm.depth_subs.push((900, false));
        farm.depth_fanout_exchange.push((900, "ARCA".into()));
        farm.depth_resub_info.push((
            7, 756733, "ARCA".into(), "STK".into(), "SPY".into(), 10, false,
        ));

        let refused = fix::fix_build(
            &[(35, "3"), (262, "900"), (58, "Error&ARCA/NO_ENTITLEMENT/depth")], 1,
        );
        farm.handle_subscription_reject(&refused, &context, &shared);

        assert!(
            !farm.depth_resub_info.iter().any(|(id, ..)| *id == 7),
            "the reconnect would ask for the refused book again and tell the \
             caller its book had been emptied first",
        );
    }
}

/// A snapshot and a stream can share the record, but only the stream's
/// selector describes the stream's entries when they are withdrawn.
#[test]
fn a_stream_beside_snapshots_is_withdrawn_with_its_own_selector() {
    for mode in [1, 2, 3] {
        let mut farm = FarmState::new();
        let mut hb = HeartbeatState::new();
        let (conn, peer) = Connection::for_test();
        let mut conn = Some(conn);
        let mut peer = Connection::new_raw(peer).unwrap();
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", 0, 0, true, &mut conn, &mut hb,
        );
        assert!(!farm.holds_a_stream(0), "the snapshot leaves room for a stream");
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", 0, mode, false, &mut conn, &mut hb,
        );
        // Read per entry: a stream states its group twice, and a map keyed by
        // the tag alone would hold only the second leg.
        let asked = drain_inner(&mut peer).into_iter()
            .flat_map(|msg| fix::fix_parse_repeating(&msg, 262))
            .find(|entry| entry.get(&264).is_some_and(|v| v == "442"))
            .expect("the quote entries went out");
        assert_eq!(asked.get(&9887), Some(&mode.to_string()));
        let stream_req_id = asked.get(&262).cloned().unwrap();
        // A later snapshot asks under a different mode, which must not change
        // how the already running stream is withdrawn.
        farm.send_mktdata_subscribe(
            756733, "SPY", "SMART", "STK", "", 0.0, "", "", 0, 4 - mode, true, &mut conn, &mut hb,
        );
        drain_inner(&mut peer);
        farm.send_mktdata_unsubscribe(0, 0, 0, &[], u64::MAX, false, &mut conn, &mut hb);
        let withdrawn = drain_inner(&mut peer).into_iter().find(|msg| {
            let tags = fix::fix_parse(msg);
            tags.get(&263).is_some_and(|v| v == "2")
                && tags.get(&262) == Some(&stream_req_id)
        }).expect("the same request is withdrawn");
        let withdrawn = fix::fix_parse(&withdrawn);
        assert_eq!(withdrawn.get(&264).map(String::as_str), Some("442"));
        assert_eq!(
            withdrawn.get(&9887), Some(&mode.to_string()),
            "the quote withdrawal keeps its selector",
        );
    }
}
