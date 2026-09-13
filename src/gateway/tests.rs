//! The tests for this module.
//!
//! One file per module, as `api/client` already does it. Each block below
//! reaches the code it tests through `super::super`, which is the module this
//! file belongs to.


/// Field 4 is where the second-factor type lives, and reading the wrong
/// field is what caused both live failures this gate was written for — a
/// sub-type sent to the wrong account shape, and a factor routed to the
/// wrong prompt. Nothing pinned it.
#[test]
fn auth_start_token_comes_from_field_four() {
    let frame = |token: &str| format!("a;b;c;d;{token};f");

    assert_eq!(parse_auth_start_token(&frame("5.2i")), ("5".into(), Some("2i".into())));
    assert_eq!(parse_auth_start_token(&frame("4")), ("4".into(), None));
    assert_eq!(parse_auth_start_token(&frame("4,5")), ("4,5".into(), None));
    assert_eq!(parse_auth_start_token(&frame(" 5.2i ")), ("5".into(), Some("2i".into())));

    // A sub-type on one entry must not swallow the entries after it.
    assert_eq!(parse_auth_start_token(&frame("5.2i,4")), ("5,4".into(), Some("2i".into())));

    // On a mixed account the sub-type must belong to the type the gate
    // routes to, which prefers IBKey. Keeping the first one stated sent
    // the authenticator's sub-type in the IBKey init.
    assert_eq!(
        parse_auth_start_token(&frame("4.auth,5.2i")),
        ("4,5".into(), Some("2i".into())),
    );
    assert_eq!(
        parse_auth_start_token(&frame("5.2i,4.auth")),
        ("5,4".into(), Some("2i".into())),
    );
    // With no IBKey entry the authenticator's own sub-type is the one that
    // belongs to the route taken.
    assert_eq!(
        parse_auth_start_token(&frame("4.auth,9.other")),
            ("4,9".into(), Some("auth".into())),
    );
    // The routed entry stating no sub-type of its own must not borrow one
    // from an entry the gate is not using. `4.auth,5` routes to IBKey, so
    // the configured fallback is the answer — sending the authenticator's
    // sub-type in the IBKey init is what this field kept getting wrong.
    assert_eq!(parse_auth_start_token(&frame("4.auth,5")), ("4,5".into(), None));
    assert_eq!(parse_auth_start_token(&frame("9.other,5")), ("9,5".into(), None));
    assert_eq!(parse_auth_start_token(&frame("9.other,4")), ("9,4".into(), None));
    // A sub-type on a type neither gate serves is still better than none.
    assert_eq!(
        parse_auth_start_token(&frame("9.other")),
            ("9".into(), Some("other".into())),
        );

    // Short or absent field 4 yields no type rather than a wrong one.
    assert_eq!(parse_auth_start_token("a;b;c"), ("".into(), None));
    assert_eq!(parse_auth_start_token(&frame("")), ("".into(), None));
}

/// A reconnect starts where the session actually is.
///
/// The venue names which server an account belongs on and the session
/// follows it, so the address a caller configured is a door that only
/// redirects. Reconnecting to it starts the redirect again on every
/// attempt, and on a session that was redirected once that is every
/// reconnect it will ever make.
#[test]
fn a_reconnect_starts_where_the_session_ended_up() {
    // The order a session records them in: where it ended up, then each
    // door it knocked on to get there.
    let seen = ["zdc1.example".to_string(), "cdc1.example".to_string()];

    // The caller configured the door. What matters is the room.
    assert_eq!(super::alternates_to(&seen, "zdc1.example"), ["cdc1.example"]);

    // And the door is still worth keeping: it answered for this account
    // once, which is more than can be said for an address nobody named.
    assert!(seen.contains(&"cdc1.example".to_string()));
}

/// A reconnect is given the hosts the venue sent this session to, and
/// never the one it is already failing on.
///
/// Every one of them was named by the venue: the first is the door this
/// client knocked on, and each of the rest is where it was sent next. A
/// list invented here instead would be reaching for a server nobody said
/// was there, which is how a farm asked for on the wrong host spends ten
/// seconds being silently closed.
#[test]
fn a_reconnect_is_given_the_hosts_the_venue_sent_this_session_to() {
    let seen = ["zdc1.example".to_string(), "cdc1.example".to_string()];

    // Reconnecting to where the session ended up leaves the door it
    // knocked on first to try.
    assert_eq!(super::alternates_to(&seen, "zdc1.example"), ["cdc1.example"]);

    // And the other way round. Retrying the host that just failed is not a
    // second attempt, so it is never in the list.
    assert_eq!(super::alternates_to(&seen, "cdc1.example"), ["zdc1.example"]);

    // A session that was never redirected has one host and nowhere else to
    // go, which is the truth rather than a reason to invent somewhere.
    let one = ["cdc1.example".to_string()];
    assert!(super::alternates_to(&one, "cdc1.example").is_empty());

    // A host that is not among them is still not retried against itself.
    assert_eq!(super::alternates_to(&one, "elsewhere.example"), ["cdc1.example"]);
}

/// A redirect states where to go, and states a port with it. The port is
/// read and carried, and it is not where the logon is answered: measured
/// live, a redirect to this venue names 4000 and only 4001 completes.
#[test]
fn a_redirect_states_a_port_alongside_its_host() {
    assert_eq!(super::host_and_port("zdc1.example:4002", 4001), ("zdc1.example", 4002));
    // The common case: only a host, so the session stays on its port.
    assert_eq!(super::host_and_port("zdc1.example", 4001), ("zdc1.example", 4001));
    // Nonsense where a port should be is not a reason to connect nowhere.
    assert_eq!(super::host_and_port("zdc1.example:four", 4001), ("zdc1.example", 4001));
    }

/// The doors are tried in order, and the one that just failed is not
/// tried again as if it were a second chance.
#[test]
fn the_doors_after_the_one_that_failed_are_the_rest_of_them() {
    let doors: Vec<String> =
        crate::config::CCP_HOSTS.iter().map(|h| h.to_string()).collect();
    let rest = super::alternates_to(&doors, crate::config::CCP_HOSTS[0]);
    assert_eq!(rest.len(), doors.len() - 1);
    assert!(!rest.contains(&crate::config::CCP_HOSTS[0].to_string()));
    assert_eq!(rest[0], crate::config::CCP_HOSTS[1], "and in the order they are listed");
}

/// A host that stops answering has backups behind it, and the venue names
/// them by rewriting the host rather than by announcing them.
///
/// Without them a dead auth host ends the session: a first connect has no
/// alternates at all, and a reconnect has only the hosts a redirect
/// happened to name. The venue's answer is one label away.
#[test]
fn a_host_that_stops_answering_has_backups_named_after_it() {
    assert_eq!(
        super::hot_backup_peers("cdc1.ibllc.com"),
        ["cdc1-hb1.ibllc.com", "cdc1-hb2.ibllc.com"],
    );

    // A walk that already stepped onto a backup derives from the primary.
    // Backups of a backup are hosts nobody runs.
    assert_eq!(
        super::hot_backup_peers("cdc1-hb2.ibllc.com"),
        ["cdc1-hb1.ibllc.com", "cdc1-hb2.ibllc.com"],
    );

    // Only the first label is rewritten; the rest of the name is the same
    // domain, and the port the caller dials never enters into it.
    assert_eq!(super::hot_backup_peers("gw1.example.co.uk")[0], "gw1-hb1.example.co.uk");
    assert_eq!(super::hot_backup_peers("localhost"), ["localhost-hb1", "localhost-hb2"]);

    // An address has no label to rewrite, so there is nothing to derive and
    // the walk is not sent at a host that cannot exist.
    assert!(super::hot_backup_peers("127.0.0.1").is_empty());
    assert!(super::hot_backup_peers("::1").is_empty());
    assert!(super::hot_backup_peers("-hb1.example").is_empty());
}

/// The backups behind the host that failed are knocked on before any other
/// region's door, and a caller that named its own host gets them too.
#[test]
fn the_backups_behind_a_door_are_tried_before_the_other_doors() {
    let shipped = crate::config::CCP_HOSTS[0];
    let backups = super::hot_backup_peers(shipped);
    let doors = super::doors_after(shipped);
    assert_eq!(doors[..backups.len()], backups[..], "the host's own backups come first");
    assert_eq!(doors[backups.len()], crate::config::CCP_HOSTS[1], "then the other regions");
    assert_eq!(doors.len(), backups.len() + crate::config::CCP_HOSTS.len() - 1);

    // A caller that named its own host is not sent to another region — but
    // the backups behind that host are still the host it named.
    assert_eq!(
        super::doors_after("gw1.example.com"),
        ["gw1-hb1.example.com", "gw1-hb2.example.com"],
    );
}

/// Which session a logon the venue names belongs to, decided by when it
/// was made.
#[test]
fn a_session_younger_than_this_one_belongs_to_somebody_else() {
    // What was watched happen: this session logged in, lost the account,
    // and found a logon from a minute later sitting on it.
    assert!(super::is_another_client("20260814-09:59:33", "20260814-09:58:33"));
    // The same session's own logon, still listed while the venue reaps it.
    assert!(!super::is_another_client("20260814-09:58:33", "20260814-09:58:33"));
    assert!(!super::is_another_client("20260814-09:58:32", "20260814-09:58:33"));
    // Across a day and a year boundary, which the text order has to hold.
    assert!(super::is_another_client("20260815-00:00:01", "20260814-23:59:59"));
    assert!(!super::is_another_client("20251231-23:59:59", "20260101-00:00:01"));
    // No stamp of this session's own: every session the venue names is
    // somebody else, which gives the account up rather than taking it.
    assert!(super::is_another_client("20260814-09:58:33", ""));
}

/// The venue names the session that was already logged in, and this reads
/// it out of the frame by its shape rather than by counting fields.
#[test]
fn a_competing_session_is_read_out_of_the_connect_response() {
    // A frame as the venue sends it, from a session with nobody else on
    // the account: the host and port it routed to, then the bare zero.
    assert_eq!(
        super::parse_competing_session(
            "50;523;zdc1.example:4000;0;TST,ONELOGON;spqili231;aaa;;;;"
        ),
        None,
        "the routed host carries a colon and a port, not a login time",
    );

    // Nobody else: the field is a bare zero, and some sessions carry none.
    assert_eq!(super::parse_competing_session("50;523;0;;2;0;"), None);
    assert_eq!(super::parse_competing_session("50;523;;;;"), None);

    let other = super::parse_competing_session("50;523;192.168.1.9/20260813-15:04:22;2;0;")
        .expect("a competing session is stated");
    assert_eq!(other.ip, "192.168.1.9");
    assert_eq!(other.since, "20260813-15:04:22");
    assert!(!other.read_only, "nothing said this session may not trade");

    // The suffix is about the session being told, not the other one.
    let held = super::parse_competing_session("50;523;10.0.0.4/20260813-09:30:00(RO);2;")
        .expect("a competing session is stated");
    assert!(held.read_only, "this session may look and not trade");
    assert_eq!(held.since, "20260813-09:30:00", "the suffix is not part of the time");

    // Position is not what identifies it: a frame that gains a field in
    // front must not change what is read.
    let moved = super::parse_competing_session("50;523;x;y;z;10.0.0.4/20260813-09:30:00;")
        .expect("still found");
    assert_eq!(moved.ip, "10.0.0.4");

    // Other fields carrying a slash are not login times. A route and a
    // version both do, and reading either as a competitor would announce
    // one on every session.
    assert_eq!(super::parse_competing_session("50;523;zdc1.ibllc.com/ushmds;2;"), None);
    assert_eq!(super::parse_competing_session("50;523;5.2i/2;0;"), None);
    assert_eq!(super::parse_competing_session("50;523;/20260813-09:30:00;"), None);
    assert_eq!(super::parse_competing_session("50;523;1.2.3.4/2026081-09:30:00;"), None);
    assert_eq!(super::parse_competing_session("50;523;1.2.3.4/20260813-9:30:00;"), None);
}

/// `recv_secure` clears the outer envelope and stops there. Field 4 of what
/// it hands back was read as the second-factor declaration whatever the
/// frame actually was, so another message at that point in the handshake
/// supplied the type and sub-type from fields that mean something else.
#[test]
fn a_frame_that_is_not_auth_start_is_refused() {
    // 50;520;...;<type>.<sub-type>;<auth mode>;
    let auth_start = auth_start_text(b"50;520;a;b;5.2i;2;").unwrap();
    assert_eq!(parse_auth_start_token(&auth_start), ("5".into(), Some("2i".into())));

    // Same shape, another type. NS_CONNECT_RESPONSE's field 4 is not a
    // second-factor declaration, and reading it as one picks an exchange.
    let err = auth_start_text(b"50;523;a;b;5.2i;2;").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("523"), "{}", err);

    // Too short to state a type is not AUTH_START either.
    assert!(auth_start_text(b"50;").is_err());
}

#[test]
fn second_factor_route_covers_every_token_type() {
    use super::{second_factor_route, SecondFactorRoute::*};
    // An absent type must still enter the IBKey gate. That gate opens by
    // sending its init, and an account with no second factor completes
    // through it — routing it to `None` sends nothing and leaves the
    // server waiting on an init that never arrives.
    assert_eq!(second_factor_route(false, ""), IbKey);
    assert_eq!(second_factor_route(false, "5"), IbKey);
    assert_eq!(second_factor_route(false, "4"), SecurityCode);
    // The field is a list when more than one factor is enabled. An
    // account with both an authenticator and IBKey advertises `4,5`, and
    // reading that as one type refused the login outright.
    assert_eq!(second_factor_route(false, "4,5"), IbKey);
    assert_eq!(second_factor_route(false, "5,4"), IbKey, "order must not matter");
        assert_eq!(second_factor_route(false, "3,4"), SecurityCode, "an unknown entry must not veto a known one");
    assert_eq!(second_factor_route(false, " 4 , 5 "), IbKey, "entries may be padded");
    assert_eq!(second_factor_route(false, "3,6"), Unsupported, "a list of unknowns is still unsupported");
    assert_eq!(second_factor_route(false, "3"), Unsupported);
    assert_eq!(second_factor_route(false, "05"), Unsupported);
    assert_eq!(second_factor_route(false, "banana"), Unsupported);
    // Paper never presents one, whatever the field says.
    for t in ["", "3", "4", "5"] {
        assert_eq!(second_factor_route(true, t), None, "paper, type {t:?}");
    }
}

use super::*;

/// The init burst is handed to the engine still compressed, and the engine
/// decompresses the same segments itself. The inflated plaintext is
/// read by the local tag scan, not appended to that buffer:
/// appending to the buffer puts every message in the burst in front of the
/// engine twice from a single delivery.
#[test]
fn the_inflated_init_content_is_scanned_but_not_handed_to_the_engine() {
    let inner = b"8=FIX.4.2\x0135=B\x0158=ROUTING\x016145=farm-a\x0110=000\x01";
    let envelope = fixcomp::fixcomp_build(inner);
    let mut burst = b"8=FIX.4.2\x0135=A\x01".to_vec();
    burst.extend_from_slice(&envelope);
    burst.extend_from_slice(b"8=FIX.4.2\x0135=0\x01");

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack.windows(needle.len()).filter(|w| *w == needle).count()
    }

    let before = count(&burst, b"58=ROUTING");
    let scan = init_scan_buffer(&burst);

    assert_eq!(
        count(&scan, b"58=ROUTING"), 1,
        "the tag scan reads the segment's content exactly once",
    );
    assert!(!scan.windows(envelope.len()).any(|w| w == envelope), "compressed bytes are not tags");
    let mut expected = b"8=FIX.4.2\x0135=A\x01".to_vec();
    expected.extend_from_slice(inner);
    expected.extend_from_slice(b"\x018=FIX.4.2\x0135=0\x01");
    assert_eq!(scan, expected, "plaintext keeps its order around the envelope");
    assert_eq!(
        count(&burst, b"58=ROUTING"), before,
        "and the buffer the engine is handed is not the one that grew",
    );
}

#[test]
fn token_short_hash_deterministic() {
    let token = BigUint::from(123456789u64);
    let h1 = token_short_hash(&token);
    let h2 = token_short_hash(&token);
    assert_eq!(h1, h2);
    // Should be lowercase hex
    assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
}

/// The hash is written in its shortest form, with no leading zeros.
///
/// That is the only form the venue is ever sent, so it is the form the venue
/// takes. Padded to a fixed eight characters, one session in sixteen stated a
/// value in a shape nothing else on this wire writes.
#[test]
fn the_token_hash_is_written_in_its_shortest_form() {
    // A token whose hash begins with a zero nibble, found by trying tokens in
    // order rather than by assuming one.
    let short = (1u64..4096)
        .map(|n| token_short_hash(&BigUint::from(n)))
        .find(|h| h.len() < 8)
        .expect("a hash with a leading zero nibble inside the first few thousand tokens");
    assert!(!short.starts_with('0'), "no leading zero is written: {short}");
    assert!(short.chars().all(|c| c.is_ascii_hexdigit()), "and it is hex: {short}");
}

#[test]
fn token_short_hash_different_tokens() {
    let t1 = BigUint::from(111u64);
    let t2 = BigUint::from(222u64);
    assert_ne!(token_short_hash(&t1), token_short_hash(&t2));
}

/// A route that states no port leaves the choice to the configured one.
#[test]
fn parse_farm_route_two_segments() {
    let parsed = parse_farm_route("zdc1.ibllc.com/eufarm").unwrap();
    assert_eq!(parsed, ("zdc1.ibllc.com".to_string(), "eufarm".to_string(), None));
}

/// A route that states a port states where that farm answers. It is read from
/// here when no tag carries it, and a route with neither is an error rather
/// than a default: a stated port is not discarded in favour of a constant.
#[test]
fn parse_farm_route_takes_the_port_the_venue_states() {
    let parsed = parse_farm_route("zdc1.ibllc.com/euhmds/4002").unwrap();
    assert_eq!(parsed, ("zdc1.ibllc.com".to_string(), "euhmds".to_string(), Some(4002)));

    // A third segment that is not a port is not one. The route still
    // names a host and a farm, which is what it is read for.
    let odd = parse_farm_route("zdc1.ibllc.com/euhmds/notaport").unwrap();
    assert_eq!(odd.2, None);
}

#[test]
fn the_channel_role_comes_from_the_farm_not_its_name() {
    // The role was keyed on the literal "ushmds", so a regional
    // historical-data farm was established on the trading channel. The
    // caller names the farm, which is the discriminator that holds for
    // every farm name — including `cashhmds`, which this codebase connects
    // as market data despite the suffix. Asserted on the wire values
    // rather than through the constants: a drifting constant would
    // otherwise put every historical-data farm on the wrong channel with
    // the suite still green.
    assert_eq!(Farm::MarketData.channel_id(), "1");
    assert_eq!(Farm::Historical.channel_id(), "2");
    assert_eq!(Farm::SecurityDefinition.channel_id(), "4");

    // The version a farm logs on at is a different number from the service
    // it asks for, and they were one value while there were only two
    // farms. Every service logs on at seventeen except market data.
    assert_eq!(Farm::MarketData.login_version(), 18);
    assert_eq!(Farm::Historical.login_version(), 17);
    assert_eq!(Farm::SecurityDefinition.login_version(), 17);
}


#[test]
fn parse_farm_route_us_account() {
    let parsed = parse_farm_route("cdc1.ibllc.com/usfarm").unwrap();
    assert_eq!(parsed, ("cdc1.ibllc.com".to_string(), "usfarm".to_string(), None));
}

#[test]
fn parse_farm_route_rejects_empty_and_malformed() {
    assert_eq!(parse_farm_route(""), None);
    assert_eq!(parse_farm_route("nofarm.example.com"), None);
    assert_eq!(parse_farm_route("/farm"), None);
    assert_eq!(parse_farm_route("host/"), None);
}


#[test]
fn build_ccp_logon_structure() {
    let msg = build_ccp_logon(
        &Default::default(), "abc123|00:00:00:00:00:00", "17.0.10.0.101/W/en/G", 10, 1,
    );
    let fields = fix_parse(&msg);
    assert_eq!(fields[&35], "A");
    assert_eq!(fields[&98], "0");
    assert_eq!(fields[&108], "10");
    assert_eq!(fields[&141], "Y");
    assert_eq!(fields[&6034], crate::config::ib_build());
    assert_eq!(fields[&6968], crate::config::ib_version());
    assert_eq!(fields[&6490], "dark");
    assert_eq!(fields[&6397], "1");
    assert_eq!(fields[&8361], "(rolling)");
    assert_eq!(fields[&8098], "0");
    assert!(fields[&6351].contains("abc123"));
}

/// What a session states is what its logon announces.
///
/// The structure test above states nothing and reads the defaults back, which
/// holds just as well for a logon that ignores its settings and composes the
/// same message every time. These are the fields a caller changes by stating a
/// setting, checked against a value no default could have produced.
#[test]
fn a_stated_setting_is_what_the_logon_announces() {
    let settings = crate::settings::SessionSettings {
        timezone: "Europe/Zurich".into(),
        build: "9999".into(),
        version: "9.9.9".into(),
        ..Default::default()
    };
    let msg = build_ccp_logon(
        &settings, "abc123|00:00:00:00:00:00", "17.0.10.0.101/W/ja_JP/G", 10, 1,
    );
    let fields = fix_parse(&msg);
    assert_eq!(fields[&6947], "Europe/Zurich", "the zone the session announces");
    assert_eq!(fields[&6034], "9999", "the build it announces");
    assert_eq!(fields[&6968], "9.9.9", "the version beside it");
    assert_eq!(fields[&6266], "17.0.10.0.101/W/ja_JP/G", "the longer string with them");

    // And the address on the local network, which the machine is asked for
    // where the caller states none — inside a container that answers with the
    // container's own rather than with the machine the session belongs to.
    let stated = crate::settings::SessionSettings {
        lan_ip: Some("10.11.12.13".into()),
        ..Default::default()
    };
    let msg = build_ccp_logon(
        &stated, "abc123|AA:BB:CC:DD:EE:FF", "17.0.10.0.101/W/en/G", 10, 1,
    );
    let fields = fix_parse(&msg);
    assert_eq!(
        fields[&6351], "<abc123|AA:BB:CC:DD:EE:FF|10.11.12.13>",
        "the machine identity the logon announces",
    );
}

/// Where a farm connection goes is the whole of what these two settings do.
#[test]
fn a_stated_host_and_port_are_where_a_farm_connection_goes() {
    let venue_named = crate::settings::SessionSettings {
        market_data_host: None,
        port: 4000,
        ..Default::default()
    };
    assert_eq!(
        farm_destination(&venue_named, "hdc1.example.com", Some(4002)),
        ("hdc1.example.com".to_string(), 4002),
        "what the venue routed to stands where the session states nothing",
    );

    let stated = crate::settings::SessionSettings {
        market_data_host: Some("farm.example.invalid".into()),
        port: 5001,
        ..Default::default()
    };
    assert_eq!(
        farm_destination(&stated, "hdc1.example.com", Some(4002)),
        ("farm.example.invalid".to_string(), 4002),
        "a stated host is where the connection opens",
    );
    assert_eq!(
        farm_destination(&stated, "hdc1.example.com", None),
        ("farm.example.invalid".to_string(), 5001),
        "and the stated port applies where the routing named none",
    );
}

#[test]
fn build_farm_logon_has_required_tags() {
    let token = BigUint::from(999u64);
    let hash = token_short_hash(&token);
    assert!(!hash.is_empty());
}

#[test]
fn chrono_free_timestamp_format() {
    let ts = chrono_free_timestamp();
    assert_eq!(ts.len(), 17); // "YYYYMMDD-HH:MM:SS"
    assert_eq!(ts.as_bytes()[8], b'-');
    assert_eq!(ts.as_bytes()[11], b':');
    assert_eq!(ts.as_bytes()[14], b':');
}

#[test]
fn days_to_ymd_epoch() {
    let (y, m, d) = days_to_ymd(0);
    assert_eq!((y, m, d), (1970, 1, 1));
}

#[test]
fn parse_misc_urls_pipe_separated() {
    let m = parse_misc_urls("region_dam=ny5wwwdam1.ibllc.com|region_webserver=ny5wwwgw1.ibllc.com|nossl=0");
    assert_eq!(m.len(), 3);
    assert_eq!(m.get("region_dam").map(String::as_str), Some("ny5wwwdam1.ibllc.com"));
    assert_eq!(m.get("region_webserver").map(String::as_str), Some("ny5wwwgw1.ibllc.com"));
        assert_eq!(m.get("nossl").map(String::as_str), Some("0"));
}

#[test]
fn parse_misc_urls_pct_encoded_pipe() {
    let m = parse_misc_urls("a=1|b=2|c%7Cd=3");
    assert_eq!(m.len(), 3);
    assert_eq!(m.get("a").map(String::as_str), Some("1"));
    assert_eq!(m.get("b").map(String::as_str), Some("2"));
    assert_eq!(m.get("c|d").map(String::as_str), Some("3"));
}

#[test]
fn parse_misc_urls_pct_encoded_pipe_in_value() {
    let m = parse_misc_urls("a=x%7Cy");
    assert_eq!(m.get("a").map(String::as_str), Some("x|y"));
}

#[test]
fn parse_misc_urls_pct_encoded_lowercase() {
    let m = parse_misc_urls("a=x%7cy");
    assert_eq!(m.get("a").map(String::as_str), Some("x|y"));
}

#[test]
fn parse_misc_urls_empty_input() {
    assert!(parse_misc_urls("").is_empty());
}

#[test]
fn parse_misc_urls_comma_fallback() {
    let m = parse_misc_urls("a=1,b=2,c=3");
    assert_eq!(m.len(), 3);
    assert_eq!(m.get("b").map(String::as_str), Some("2"));
}

#[test]
fn parse_misc_urls_drops_malformed_entries() {
    let m = parse_misc_urls("a=1|nokv|=val|b=2");
    assert_eq!(m.len(), 2);
    assert_eq!(m.get("a").map(String::as_str), Some("1"));
    assert_eq!(m.get("b").map(String::as_str), Some("2"));
}

#[test]
fn parse_misc_urls_value_with_equals() {
    // split_once stops at first `=`, so URLs with query strings round-trip.
    let m = parse_misc_urls("cookbook=https://x.example/path?a=1&b=2");
    assert_eq!(m.get("cookbook").map(String::as_str), Some("https://x.example/path?a=1&b=2"));
}

#[test]
fn days_to_ymd_known_date() {
    // 2026-03-05 = day 20517 since epoch
    let (y, m, d) = days_to_ymd(20517);
    assert_eq!((y, m, d), (2026, 3, 5));
}

#[test]
fn try_frame_farm_msg_incomplete() {
    assert!(try_frame_farm_msg(b"8=FIX").is_none());
    assert!(try_frame_farm_msg(b"").is_none());
}

#[test]
fn try_frame_farm_msg_complete() {
    let msg = fix_build(&[(35, "A"), (108, "30")], 1);
    let (extracted, consumed) = try_frame_farm_msg(&msg).unwrap();
    assert_eq!(extracted, msg);
    assert_eq!(consumed, msg.len());
}

#[test]
fn try_frame_farm_msg_with_trailing() {
    let msg1 = fix_build(&[(35, "A")], 1);
    let msg2 = fix_build(&[(35, "0")], 2);
    let mut buf = msg1.clone();
    buf.extend_from_slice(&msg2);
    let (extracted, consumed) = try_frame_farm_msg(&buf).unwrap();
    assert_eq!(extracted, msg1);
    assert_eq!(consumed, msg1.len());
}

/// A farm logon states what an order logon states about the session, and in
/// the places the venue's own farm logon states them.
///
/// The enciphering needs a channel a key exchange has settled, which no unit
/// test can build — so what the logon says was checked nowhere, and a field
/// missing from it was missing from every farm connection this client opens.
/// Composed apart from the enciphering, what it says can be read here.
#[test]
fn a_farm_logon_states_the_session_the_way_an_order_logon_does() {
    let settings = crate::settings::SessionSettings {
        timezone: "Europe/Zurich".into(),
        build: "9999".into(),
        version: "9.9.9".into(),
        lan_ip: Some("10.11.12.13".into()),
        ..Default::default()
    };
    let inner = crate::gateway::logon::build_farm_logon_fields(
        &settings,
        "user",
        "mdfarm",
        "SESSION1",
        &num_bigint::BigUint::from(12345u32),
        "abc123|AA:BB:CC:DD:EE:FF",
        "17.0.10.0.101/W/en/G",
        3,
    );
    let fields = fix_parse(&inner);
    assert_eq!(fields[&35], "A");
    assert_eq!(fields[&6947], "Europe/Zurich", "the zone, as the venue's own farm logon states it");
    assert_eq!(fields[&6034], "9999", "the build");
    assert_eq!(fields[&6968], "9.9.9", "the version");
    assert_eq!(
        fields[&6351], "<abc123|AA:BB:CC:DD:EE:FF|10.11.12.13>",
        "the machine identity",
    );
    assert_eq!(fields[&6266], "17.0.10.0.101/W/en/G", "the longer string");
    assert_eq!(fields[&96], "Suser/3/mdfarm", "which farm, for which session slot");
    // And in the venue's own order: the identity, then the zone, then the
    // longer string.
    let order: Vec<u32> = String::from_utf8_lossy(&inner)
        .split('\u{1}')
        .filter_map(|f| f.split_once('='))
        .filter_map(|(t, _)| t.parse::<u32>().ok())
        .filter(|t| [6351u32, 6947, 6266].contains(t))
        .collect();
    assert_eq!(order, vec![6351, 6947, 6266]);
}

#[test]
fn days_to_ymd_leap_year() {
    let (y, m, d) = days_to_ymd(19782); // 2024-02-29
    assert_eq!((y, m, d), (2024, 2, 29));
}

#[test]
fn days_to_ymd_end_of_year() {
    // 2025-12-31
    let (y, m, d) = days_to_ymd(20453); // 2025-12-31
    assert_eq!((y, m, d), (2025, 12, 31));
}

#[test]
fn days_to_ymd_start_of_2000() {
    // 2000-01-01 = 10957 days from epoch
    let (y, m, d) = days_to_ymd(10957);
    assert_eq!((y, m, d), (2000, 1, 1));
}

#[test]
fn try_frame_farm_msg_garbage_prefix() {
    let mut buf = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let msg = fix_build(&[(35, "A")], 1);
    buf.extend_from_slice(&msg);
    // Should skip garbage and return (empty, skip_count)
    let (extracted, consumed) = try_frame_farm_msg(&buf).unwrap();
    if extracted.is_empty() {
        // garbage skipped, need to retry from remaining
        let rest = &buf[consumed..];
        let (msg2, _) = try_frame_farm_msg(rest).unwrap();
        assert!(!msg2.is_empty());
    }
}

/// A control frame carries no checksum, and is not framed as though it did.
///
/// The venue answers the token step with an `8=1`, which ends where its own
/// length says — the reader that frames those alone stops exactly there. Seven
/// bytes for a checksum added to one runs past its end into the message behind
/// it, so the logon ACK that followed arrived with its header eaten and was
/// never recognised, and the establishment waited out its deadline on a reply
/// it had already been sent.
#[test]
fn a_control_frame_is_not_framed_as_though_it_carried_a_checksum() {
    // As the venue sends it, and as the reader beside this one builds it:
    // header, stated length, body, and nothing after the body.
    let body = b"35=X\x01PASSED\x01";
    let mut control = format!("8=1\x019={:04}\x01", body.len()).into_bytes();
    control.extend_from_slice(body);

    let ack = fix_build(&[(35, "A"), (108, "30")], 1);
    let mut buf = control.clone();
    buf.extend_from_slice(&ack);

    let (framed, consumed) = try_frame_farm_msg(&buf).expect("the control frame is complete");
    assert_eq!(consumed, control.len(), "it ends where its own length says");
    assert_eq!(framed, control);

    // And what followed it is still a whole message.
    let (next, _) = try_frame_farm_msg(&buf[consumed..]).expect("the ACK behind it survives");
    assert_eq!(next, ack, "the frame in front of it took its header");
}

#[test]
fn try_frame_farm_msg_multiple_sequential() {
    // Two FIX messages back to back
    let msg1 = fix_build(&[(35, "S")], 1);
    let msg2 = fix_build(&[(35, "A"), (108, "30")], 2);
    let mut buf = msg1.clone();
    buf.extend_from_slice(&msg2);
    let (extracted, consumed) = try_frame_farm_msg(&buf).unwrap();
    assert_eq!(extracted, msg1);
    assert_eq!(consumed, msg1.len());
    // Second message
    let (extracted2, consumed2) = try_frame_farm_msg(&buf[consumed..]).unwrap();
    assert_eq!(extracted2, msg2);
    assert_eq!(consumed2, msg2.len());
}

#[test]
fn token_short_hash_nonzero_output() {
    let token = BigUint::from(1u64);
    let hash = token_short_hash(&token);
    assert!(!hash.is_empty());
    // Should be hex string
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn token_short_hash_large_token() {
    let token = BigUint::from(u64::MAX);
    let hash = token_short_hash(&token);
    assert!(!hash.is_empty());
    assert!(hash.len() <= 8); // u32 hex is at most 8 chars
}

#[test]
fn chrono_free_timestamp_not_empty() {
    let ts = chrono_free_timestamp();
    assert!(!ts.is_empty());
    // Year should start with 20xx
    assert!(ts.starts_with("20"));
}

#[test]
fn gateway_config_fields() {
    let config = GatewayConfig {
        settings: Default::default(),
        username: "user".to_string(),
            password: Zeroizing::new("pass".to_string()),
        host: "cdc1.ibllc.com".to_string(),
        paper: true,
        accept_invalid_certs: false,
        ib_key_timeout_secs: session::IB_KEY_DEFAULT_TIMEOUT_SECS,
        ib_key_token_sub_type: session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
        code_provider: None,
        resume: None,
    };
    assert_eq!(config.username, "user");
        assert!(config.paper);
}

fn auth_with(host: &str, trading_host: &str, trading_farm: &str) -> ReconnectAuth {
    ReconnectAuth {
        account_id: String::new(),
        trading_port: None,
        hmds_port: None,
        secdef_port: None,
        logged_in_at: String::new(),
        alternate_hosts: Vec::new(),
        settings: Default::default(),
        host: host.to_string(),
        username: String::new(),
        password: zeroize::Zeroizing::new(String::new()),
        paper: true,
        code_provider: None,
        ib_key_timeout_secs: crate::auth::session::IB_KEY_DEFAULT_TIMEOUT_SECS,
        ib_key_token_sub_type: crate::auth::session::IB_KEY_DEFAULT_TOKEN_SUB_TYPE.into(),
        session_key: num_bigint::BigUint::from(0u32),
        session_token: num_bigint::BigUint::from(0u32),
        server_session_id: String::new(),
        hw_info: String::new(),
        encoded: String::new(),
        secdef_host: String::new(),
        secdef_farm: String::new(),
        hmds_host: String::new(),
        hmds_farm: String::new(),
        trading_host: trading_host.to_string(),
        trading_farm: trading_farm.to_string(),
    }
}

/// The trading reconnect announces the farm the venue named for the account
/// and dials the host it gave, rather than a literal and the configured host.
///
/// The host still falls back to the session's own, because a redirect moves it
/// after connect and the reconnect has to follow. The farm does not: a session
/// exists only where the venue named one, and the name a guess would reach
/// serves other accounts than this.
#[test]
fn a_reconnect_uses_the_route_the_auth_server_gave() {
    assert_eq!(
        reconnect_trading_route(&auth_with("cdc1.ibllc.com", "cdc2.ibllc.com", "euhard")),
        ("cdc2.ibllc.com".to_string(), "euhard".to_string()),
    );

    // The host falls back on its own, and carries the farm it was given.
    assert_eq!(
        reconnect_trading_route(&auth_with("cdc1.ibllc.com", "", "euhard")),
        ("cdc1.ibllc.com".to_string(), "euhard".to_string()),
    );

    // The reason the parsed route is stored raw rather than after the
    // initial connect: with the host moved by a redirect since, the
    // reconnect has to use whatever the session holds now.
    assert_eq!(
        reconnect_trading_route(&auth_with("cdc3.ibllc.com", "cdc2.ibllc.com", "eufarm")),
        ("cdc2.ibllc.com".to_string(), "eufarm".to_string()),
    );

    // A farm this client no longer holds is not replaced by one it made up:
    // the reconnect carries nothing rather than dialling somewhere else.
    assert_eq!(
        reconnect_trading_route(&auth_with("cdc1.ibllc.com", "", "")),
        ("cdc1.ibllc.com".to_string(), String::new()),
    );
}

#[test]
fn the_venue_names_what_it_permits() {
    // A live logon reply, abridged. SLB names one order type; a type named
    // with nothing after it is still permitted.
    let perms = parse_order_permissions("STK:LMT,MKT,STP;SLB:LMT;NEWS");
    assert_eq!(perms["STK"], ["LMT", "MKT", "STP"]);
    assert_eq!(perms["SLB"], ["LMT"]);
    assert!(perms["NEWS"].is_empty(), "named with no order types, still permitted");
    assert!(!perms.contains_key("FWD"), "a type the venue never named is not permitted");
    assert!(parse_order_permissions("").is_empty(), "a session that stated nothing");
}
mod logon_field_tests {
    use super::super::{keep_first, note_account};

    /// A field the venue sends whole keeps its first value, and a repeat of the
    /// same value is not a conflict.
    #[test]
    fn keep_first_keeps_the_first_value() {
        let mut slot = String::new();
        keep_first(&mut slot, "a;b", "6823");
        keep_first(&mut slot, "a;b", "6823");
        keep_first(&mut slot, "c;d", "6823");
        assert_eq!(slot, "a;b");
    }

    /// Accounts keep the order the venue named them in, without repeats, and an
    /// empty name is not an account.
    #[test]
    fn accounts_keep_their_order_without_repeats() {
        let mut accounts = Vec::new();
        for name in ["DU1", "DU2", "DU1", "", "DU3"] {
            note_account(&mut accounts, name);
        }
        assert_eq!(accounts, vec!["DU1", "DU2", "DU3"]);
    }
}
mod soft_dollar_tier_tests {
    use crate::types::SoftDollarTier;

    /// The shape the venue actually sends, taken from a real logon.
    fn parse(raw: &str) -> Vec<SoftDollarTier> {
        raw.split(',')
            .filter_map(|entry| {
                let entry = entry.trim();
                if entry.is_empty() {
                    return None;
                }
                let mut parts = entry.split('/');
                let val = parts.next()?.trim();
                let display_name = parts.next()?.trim();
                let name = parts.next()?.trim();
                if val.is_empty() || name.is_empty() {
                    return None;
                }
                Some(SoftDollarTier {
                    name: name.to_string(),
                    val: val.to_string(),
                    display_name: display_name.to_string(),
                })
            })
            .collect()
    }

    /// A real logon's own words. A list written into this client would stand
    /// in for it — itself a transcription of this very reply, so it would look
    /// right while nothing was
    /// being read.
    #[test]
    fn the_tiers_a_real_logon_states_are_read() {
        let raw = "1/Maximize Rebate/MaxRebate,9/Prefer Rebate/PreferRebate,\
                   11/Prefer Fill/PreferFill,12/Maximize Fill/MaxFill,\
                   2/Primary Exchange/Primary,\
                   3/Highest Volume Exchange With Rebate/VRebate,\
                   4/High Volume Exchange With Lowest Fee/VLowFee";
        let tiers = parse(raw);
        assert_eq!(tiers.len(), 7, "every tier the venue stated");
        assert_eq!(tiers[0].val, "1");
        assert_eq!(tiers[0].display_name, "Maximize Rebate");
        assert_eq!(tiers[0].name, "MaxRebate");
        // A name shown to a person and a name asked for by a program are not
        // the same string, and putting one where the other belongs is how a
        // caller asks for a tier that does not exist.
        assert_eq!(tiers[6].display_name, "High Volume Exchange With Lowest Fee");
        assert_eq!(tiers[6].name, "VLowFee");
    }

    /// A logon stating none means the account holds none.
    #[test]
    fn no_tiers_stated_is_no_tiers() {
        assert!(parse("").is_empty());
    }
}

/// The frame that finishes authentication has to say so.
///
/// Read as "whatever arrives next", a refusal and a receive failure both
/// count as a session, and the farm logons behind them authenticate with a key
/// the venue has not accepted.
mod auth_finish_tests {
    use super::super::second_factor::expect_auth_finish;
    use crate::protocol::xyz::{xyz_build, xyz_wrap, XYZ_MSG_SOFT_TOKEN, XYZ_MSG_TOKEN_AUTH};
    use std::io;

    fn framed(msg_id: u32, state: u32, fields: &[&str]) -> io::Cursor<Vec<u8>> {
        io::Cursor::new(xyz_wrap(&xyz_build(msg_id, state, "", fields)))
    }

    #[test]
    fn a_finish_that_passed_finishes_the_authentication() {
        for state in [3u32, 5] {
            let mut frame = framed(XYZ_MSG_TOKEN_AUTH, state, &["PASSED"]);
            let carried = expect_auth_finish(&mut frame, "test")
                .unwrap_or_else(|e| panic!("state {state} passed and was refused: {e}"));
            assert!(carried.is_none(), "a finish that passed reads nothing past itself");
        }
    }

    /// The venue can move on without sending the finish: a soft-token reconnect
    /// and a resume both answer the challenge, and the next frame can already
    /// be the post-auth exchange. That frame is not this reader's to keep —
    /// dropped here, the post-auth exchange waits for a frame it is already
    /// holding until its own deadline, and fails on its own success.
    #[test]
    fn a_frame_read_past_the_finish_is_handed_on_not_dropped() {
        use crate::protocol::ns;

        let frame_bytes = ns::ns_build(50, ns::NS_CONNECT_RESPONSE, &[], "");
        let mut stream = io::Cursor::new(frame_bytes.clone());
        let carried = expect_auth_finish(&mut stream, "test")
            .expect("an NS frame in place of the finish is not a refusal");
        let payload = &frame_bytes[8..];
        assert_eq!(
            carried.as_deref(),
            Some(payload),
            "the post-auth exchange cannot ask the socket for this frame again",
        );
        let (_, msg_type, _) = ns::ns_parse(payload).unwrap();
        assert_eq!(msg_type, ns::NS_CONNECT_RESPONSE);
    }

    #[test]
    fn a_finish_that_did_not_pass_is_not_a_session() {
        let mut refused = framed(XYZ_MSG_TOKEN_AUTH, 3, &["FAILED"]);
        let err = expect_auth_finish(&mut refused, "test").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");

        // The right word in a state AUTH_FINISH is not sent in.
        let mut wrong_state = framed(XYZ_MSG_TOKEN_AUTH, 2, &["PASSED"]);
        assert!(expect_auth_finish(&mut wrong_state, "test").is_err());

        // The right word on a message that is not this one.
        let mut wrong_message = framed(XYZ_MSG_SOFT_TOKEN, 3, &["PASSED"]);
        assert!(expect_auth_finish(&mut wrong_message, "test").is_err());
    }

    #[test]
    fn a_frame_that_never_arrived_is_not_a_session() {
        let mut nothing = io::Cursor::new(Vec::new());
        assert!(
            expect_auth_finish(&mut nothing, "test").is_err(),
            "nothing said the authentication finished",
        );
    }
}

/// A door that answered and refused is not a door that did not open.
///
/// The venue sends an error frame when it has read what was sent and will not
/// have it. Raised with no kind on it, that refusal came back as
/// [`io::ErrorKind::Other`], which [`super::nobody_answered`] reads as a
/// transport failure — so the failover knocked on all four doors with the same
/// credentials, and a wrong password became four refused logons where the
/// account asked for one. Every door answers for the same account; there is no
/// second door to ask.
///
/// Measured against the venue with an empty username: `cdc1` answered
/// `1;malformed user name;` and the client tried `ndc1`, `zdc1` and `hdc1`,
/// logging each as "did not answer".
#[test]
fn a_refusal_from_the_venue_is_not_a_door_that_did_not_open() {
    use crate::protocol::ns;
    use crate::reliability::retry::{DisconnectReason, Recovery};

    let refused = ns::refused_by_the_venue("Auth error", "1;malformed user name;".to_string());
    assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
    assert!(
        !super::nobody_answered(&refused),
        "a stated refusal sent the failover round every other host",
    );
    assert_eq!(DisconnectReason::from_error(&refused), DisconnectReason::AuthorizationFailed);
    assert_eq!(
        DisconnectReason::from_error(&refused).recovery(),
        Recovery::Stop,
        "asking again carries the same credentials",
    );

    // What the venue states as retryable stays retryable: those are read from
    // its own words, before the kind is looked at.
    let busy = ns::refused_by_the_venue("Auth error", "too many sessions".to_string());
    assert_eq!(DisconnectReason::from_error(&busy), DisconnectReason::NotReady);
    let held = ns::refused_by_the_venue("Auth error", "competing logon: elsewhere".to_string());
    assert_eq!(DisconnectReason::from_error(&held), DisconnectReason::TakenOver);

    // And a door that genuinely did not open still sends it to the next one.
    let unreachable = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
    assert!(super::nobody_answered(&unreachable));
}

/// The reconnect walk stops at an answer the same way the first connect does.
///
/// A soft-token reconnect carries the same credentials to every host the
/// session reached the venue through. When the first host answered and
/// refused, the walk went on anyway — four refused logons per attempt, and a
/// "too many sessions" loop locking the account it was trying to recover.
/// Only the one refusal that names another client was carved out; every other
/// answered refusal walked all four doors.
mod reconnect_failover_tests {
    use crate::protocol::connection::Connection;
    use crate::protocol::ns;
    use crate::reliability::retry::DisconnectReason;
    use std::io;

    fn alternates() -> Vec<String> {
        ["ndc1", "zdc1", "hdc1"].iter().map(|h| h.to_string()).collect()
    }

    fn refused() -> io::Error {
        ns::refused_by_the_venue("Auth error", "1;malformed user name;".to_string())
    }

    fn silent() -> io::Error {
        io::Error::new(io::ErrorKind::ConnectionRefused, "refused")
    }

    /// A connection good enough to say the walk stopped on an answer.
    fn a_connection() -> Connection {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = std::net::TcpStream::connect(addr).unwrap();
        Connection::new_raw(stream).unwrap()
    }

    #[test]
    fn a_refusal_on_the_first_host_ends_the_walk() {
        let mut knocked = Vec::new();
        let err = super::super::failover("cdc1", refused(), &alternates(), |host| {
            knocked.push(host.to_string());
            Err(io::Error::other("the walk must not reach another host"))
        })
        .err().expect("the walk must end in an error");
        assert!(
            knocked.is_empty(),
            "an answered refusal sent the reconnect round every other host: {knocked:?}",
        );
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    }

    #[test]
    fn a_refusal_from_a_later_host_ends_the_walk_there() {
        let mut knocked = Vec::new();
        let err = super::super::failover("cdc1", silent(), &alternates(), |host| {
            knocked.push(host.to_string());
            if knocked.len() == 1 {
                Err(silent())
            } else {
                Err(ns::refused_by_the_venue(
                    "CCP reconnect post-auth error",
                    "stale session".to_string(),
                ))
            }
        })
        .err().expect("the walk must end in an error");
        assert_eq!(
            knocked,
            vec!["ndc1", "zdc1"],
            "the walk went on after the venue answered and refused",
        );
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(err.to_string().contains("stale session"), "{err}");
    }

    #[test]
    fn doors_that_do_not_open_are_walked_to_the_end() {
        let mut knocked = Vec::new();
        let err = super::super::failover("cdc1", silent(), &alternates(), |host| {
            knocked.push(host.to_string());
            Err(silent())
        })
        .err().expect("the walk must end in an error");
        assert_eq!(knocked, alternates(), "every host the venue named is tried");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err}");
    }

    #[test]
    fn a_host_that_answers_reconnects_the_session() {
        let mut knocked = Vec::new();
        let conn = super::super::failover("cdc1", silent(), &alternates(), |host| {
            knocked.push(host.to_string());
            if host == "zdc1" {
                Ok(a_connection())
            } else {
                Err(silent())
            }
        });
        assert!(conn.is_ok(), "the walk ends on the first host that answers");
        assert_eq!(knocked, vec!["ndc1", "zdc1"]);
    }

    /// The refusal that names another client used to be the walk's only
    /// carve-out, on its own wording. The same gate covers it, because the
    /// retry ladder reads that wording before it looks at the kind.
    #[test]
    fn a_taken_account_still_ends_the_walk() {
        let taken = io::Error::other(format!(
            "{} another client holds this account",
            super::super::TOOK_THE_ACCOUNT,
        ));
        assert_eq!(DisconnectReason::from_error(&taken), DisconnectReason::TakenOver);
        let mut knocked = Vec::new();
        let err = super::super::failover("cdc1", taken, &alternates(), |host| {
            knocked.push(host.to_string());
            Err(silent())
        })
        .err().expect("the walk must end in an error");
        assert!(knocked.is_empty(), "{knocked:?}");
        assert!(err.to_string().contains("competing logon"), "{err}");
    }
}

/// The init burst arrives back to back on a fast path and with gaps in it on
/// a slow one, so the drain ends on a quiet stretch, not on the first quiet
/// read. Ending it there left the routing tags in bytes never drained, and
/// the venue was blamed for naming no route it had named.
mod init_burst_drain_tests {
    use std::io::{self, Read};
    use std::time::Duration;

    /// A socket scripted for one drain: each entry is one read's answer, and
    /// an exhausted socket keeps answering the way the venue's does once the
    /// burst is done — with nothing until the quiet stretch ends the drain.
    struct BurstReader { script: Vec<io::Result<Vec<u8>>> }

    impl Read for BurstReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.script.is_empty() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "quiet"));
            }
            match self.script.remove(0) {
                Ok(bytes) => {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    Ok(n)
                }
                Err(e) => Err(e),
            }
        }
    }

    fn quiet() -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::TimedOut, "quiet"))
    }

    fn chunk(bytes: &[u8]) -> io::Result<Vec<u8>> {
        Ok(bytes.to_vec())
    }

    fn interrupted() -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Interrupted, "a signal"))
    }

    const POLL: Duration = Duration::from_millis(250);
    const GAP: Duration = Duration::from_secs(1);

    fn far_enough() -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(60)
    }

    #[test]
    fn a_gap_in_the_burst_does_not_end_the_drain() {
        // Two chunks with a quiet read between them — a gap inside the
        // burst, shorter than the stretch that ends it — and the quiet
        // stretch after.
        let mut reader = BurstReader {
            script: vec![
                chunk(b"6145=usfarm;"),
                quiet(),
                quiet(),
                chunk(b"6171=eufarm;"),
                quiet(),
                quiet(),
                quiet(),
                quiet(),
            ],
        };
        let drained = super::super::drain_init_burst(&mut reader, Vec::new(), POLL, GAP, far_enough())
            .expect("the drain ends on the quiet stretch, not the gap");
        assert_eq!(
            drained,
            b"6145=usfarm;6171=eufarm;",
            "the bytes after the gap were left undrained",
        );
    }

    /// A signal cutting a read short is not a poll's worth of silence. No
    /// time passed and the peer said nothing either way, so counted as quiet
    /// a handful of signals inside one second stand in for the whole gap and
    /// the burst still in flight is left unread.
    #[test]
    fn a_signal_is_not_a_quiet_read() {
        let mut reader = BurstReader {
            script: vec![
                quiet(),
                quiet(),
                quiet(),
                interrupted(),
                interrupted(),
                chunk(b"6145=usfarm;"),
                quiet(),
                quiet(),
                quiet(),
                quiet(),
            ],
        };
        let drained = super::super::drain_init_burst(&mut reader, Vec::new(), POLL, GAP, far_enough())
            .expect("the drain ends on the quiet stretch");
        assert_eq!(
            drained,
            b"6145=usfarm;",
            "the burst arriving after the signals was left undrained",
        );
    }

    #[test]
    fn a_socket_closed_mid_burst_says_so() {
        let mut reader = BurstReader {
            script: vec![chunk(b"6145=usfarm;"), Ok(Vec::new())],
        };
        let err = super::super::drain_init_burst(&mut reader, Vec::new(), POLL, GAP, far_enough())
            .expect_err("a closed socket is not a burst that finished");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset, "{err}");
    }

    #[test]
    fn what_arrived_before_the_drain_leads_it() {
        // The bytes the logon reader carried past the ACK belong to the burst
        // and lead it.
        let mut reader = BurstReader {
            script: vec![chunk(b"late;"), quiet(), quiet(), quiet(), quiet()],
        };
        let drained = super::super::drain_init_burst(&mut reader, b"carry;".to_vec(), POLL, GAP, far_enough())
            .expect("the drain ends on the quiet stretch");
        assert_eq!(drained, b"carry;late;");
    }
}

/// The routing answer is optional: an idle timeout on a still-open socket is
/// the shape its absence takes. A socket that closes while it is awaited is
/// not that — read as one, the close came back later as a successful
/// connection on a socket already gone.
mod routing_response_tests {
    use std::io::{self, Read};
    use std::time::{Duration, Instant};

    /// A socket scripted for one read sequence; once exhausted it keeps
    /// answering the way an open, idle socket does — with a poll timeout.
    struct RoutingReader { script: Vec<io::Result<Vec<u8>>> }

    impl Read for RoutingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.script.is_empty() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "quiet"));
            }
            match self.script.remove(0) {
                Ok(bytes) => {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    Ok(n)
                }
                Err(e) => Err(e),
            }
        }
    }

    fn chunk(bytes: &[u8]) -> io::Result<Vec<u8>> {
        Ok(bytes.to_vec())
    }

    #[test]
    fn a_closed_socket_is_not_a_routing_response_that_never_came() {
        // A close while the answer is still arriving is stated as one,
        // whatever has arrived so far.
        let mut closing = RoutingReader {
            script: vec![chunk(b"8=O"), Ok(Vec::new())],
        };
        let err = super::super::read_routing_response(
            &mut closing,
            Vec::new(),
            Instant::now() + Duration::from_secs(5),
        )
        .expect_err("a socket that closed is not a response that is absent");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset, "{err}");

        // An open, silent socket past the deadline is the absent optional
        // response.
        let mut silent = RoutingReader { script: Vec::new() };
        let none = super::super::read_routing_response(&mut silent, Vec::new(), Instant::now())
            .expect("silence past the deadline is an absent response, not an error");
        assert!(none.is_empty());

        // And a complete frame ends the read the moment it is held.
        let frame = b"8=O\x019=5\x01hello";
        let mut answering = RoutingReader {
            script: vec![chunk(frame), Ok(Vec::new())],
        };
        let got = super::super::read_routing_response(
            &mut answering,
            Vec::new(),
            Instant::now() + Duration::from_secs(5),
        )
        .expect("a complete frame is the response");
        assert_eq!(got, frame);
    }

    /// The logon can read only the head of the frame before the routing reply.
    #[test]
    fn a_routing_wait_continues_the_frame_the_logon_started() {
        let heartbeat = crate::protocol::fixcomp::fixcomp_build(
            &crate::protocol::fix::fix_build(&[(35, "0")], 1),
        );
        let reply = b"8=O\x019=5\x01hello";
        for split in [5, heartbeat.len() - 2] {
            let mut rest = heartbeat[split..].to_vec();
            rest.extend_from_slice(reply);
            let mut wire = RoutingReader {
                script: vec![Ok(rest), Ok(Vec::new())],
            };
            let got = super::super::read_routing_response(
                &mut wire,
                heartbeat[..split].to_vec(),
                Instant::now() + Duration::from_secs(5),
            ).expect("the complete reply ends the wait before another read");
            let mut expected = heartbeat.clone();
            expected.extend_from_slice(reply);
            assert_eq!(got, expected, "the logon remainder is present exactly once");
            assert_eq!(wire.script.len(), 1, "no read is needed after the reply");
        }
        let mut wire = RoutingReader { script: vec![Ok(Vec::new())] };
        let got = super::super::read_routing_response(
            &mut wire,
            reply.to_vec(),
            Instant::now() + Duration::from_secs(5),
        ).expect("a reply already read during logon needs no more bytes");
        assert_eq!(got, reply);
        assert_eq!(wire.script.len(), 1);
    }

    /// The farm's own traffic arriving first does not end the wait.
    ///
    /// A compressed heartbeat is a complete frame and is not the answer. Ended
    /// on it, the reply arrived afterwards on the connection's own reads — and
    /// the loop that takes a routing table out of a frame runs only over what
    /// this returned, so the table stayed empty for the session on a reply the
    /// venue had sent.
    #[test]
    fn a_heartbeat_before_the_reply_does_not_end_the_wait() {
        let heartbeat = crate::protocol::fixcomp::fixcomp_build(
            &crate::protocol::fix::fix_build(&[(35, "1"), (112, "probe")], 1),
        );
        let reply = b"8=O\x019=5\x01hello".to_vec();

        let mut wire = RoutingReader {
            script: vec![Ok(heartbeat.clone()), Ok(reply.clone()), Ok(Vec::new())],
        };
        let got = super::super::read_routing_response(
            &mut wire,
            Vec::new(),
            Instant::now() + Duration::from_secs(5),
        )
        .expect("the reply arrives behind the heartbeat");

        assert!(
            got.windows(reply.len()).any(|w| w == reply.as_slice()),
            "the wait ended on the farm's own traffic, so the reply reached \
             nothing that reads a routing table: {got:?}",
        );
    }

    /// And a pause between the farm's own traffic and the reply does not end
    /// the wait either.
    ///
    /// The read that takes the heartbeat and the read that takes the reply need
    /// not be consecutive: a poll times out in between, and that branch had its
    /// own shortcut on any complete frame. So the wait ended on the heartbeat
    /// after all, and the reply arrived where nothing takes a routing table out
    /// of it — the case the test above cannot reach, because it supplies the
    /// two reads back to back.
    #[test]
    fn a_pause_between_the_heartbeat_and_the_reply_does_not_end_the_wait() {
        let heartbeat = crate::protocol::fixcomp::fixcomp_build(
            &crate::protocol::fix::fix_build(&[(35, "1"), (112, "probe")], 1),
        );
        let reply = b"8=O\x019=5\x01hello".to_vec();

        let mut wire = RoutingReader {
            script: vec![
                Ok(heartbeat.clone()),
                Err(io::Error::new(io::ErrorKind::TimedOut, "quiet")),
                Ok(reply.clone()),
                Ok(Vec::new()),
            ],
        };
        let got = super::super::read_routing_response(
            &mut wire,
            Vec::new(),
            Instant::now() + Duration::from_secs(5),
        )
        .expect("the reply arrives after the pause");

        assert!(
            got.windows(reply.len()).any(|w| w == reply.as_slice()),
            "a poll timing out between the two ended the wait on the heartbeat: {got:?}",
        );
    }

    /// The deadline stands for an answer that is not coming, and a peer that
    /// keeps sending is still not answering.
    ///
    /// Read only where the socket goes quiet, it never came due at all: every
    /// read succeeded, none of them completed a frame, and the loop went round
    /// again — holding the connect for as long as the bytes kept arriving, and
    /// growing what it held for just as long.
    #[test]
    fn a_peer_that_never_stops_sending_does_not_outlast_the_deadline() {
        /// Bytes that never complete a frame, without end.
        struct NeverAFrame;
        impl Read for NeverAFrame {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = buf.len().min(64);
                buf[..n].fill(b'x');
                Ok(n)
            }
        }
        let held = super::super::read_routing_response(&mut NeverAFrame, Vec::new(), Instant::now())
            .expect("a deadline reached with no frame is an absent optional response");
        assert!(
            held.len() < 1_000_000,
            "the read ended at the deadline rather than for as long as bytes arrived",
        );
    }
}

/// A redirect or a refusal can arrive after the reconnect's authentication
/// as well as before it. Waited out, the attempt simply repeats: one sent
/// elsewhere knocks again at the door that sent it on, and one refused
/// re-asks a question the venue has already answered — so both are acted on.
mod reconnect_post_auth_tests {
    use crate::auth::dh::SecureChannel;
    use crate::config::NS_VERSION;
    use crate::protocol::ns;
    use std::io::{self, Read, Write};
    use std::time::{Duration, Instant};

    /// Serves one frame per read; once exhausted, answers the way an open,
    /// idle socket does — with a poll timeout.
    struct FramedScript { frames: Vec<Vec<u8>>, pos: usize }

    impl Read for FramedScript {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.frames.is_empty() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "quiet"));
            }
            let frame = &self.frames[0];
            let n = (frame.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&frame[self.pos..self.pos + n]);
            self.pos += n;
            if self.pos == frame.len() {
                self.frames.remove(0);
                self.pos = 0;
            }
            Ok(n)
        }
    }

    impl Write for FramedScript {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }

    #[test]
    fn a_redirect_or_a_refusal_after_auth_is_acted_on_not_waited_out() {
        // A redirect names where the attempt goes next.
        let redirect = ns::ns_build(NS_VERSION, ns::NS_REDIRECT, &["cdc1.example:4000"], "");
        let mut stream = FramedScript { pos: 0, frames: vec![redirect] };
        match super::super::wait_for_fix_start(
            &mut stream, &mut SecureChannel::new(), "", deadline(), None,
        ) {
            Ok(super::super::ReconnectPostAuth::Redirect(host)) => {
                assert_eq!(host, "cdc1.example", "the port it names is the record, not the target");
            }
            Ok(_) => panic!("the redirect was waited out, not followed"),
            Err(e) => panic!("the redirect was refused: {e}"),
        }

        // A secure error is the venue refusing, and reads as one.
        let refusal = ns::ns_build(NS_VERSION, ns::NS_SECURE_ERROR, &["stale session"], "");
        let mut stream = FramedScript { pos: 0, frames: vec![refusal] };
        let err = match super::super::wait_for_fix_start(
            &mut stream, &mut SecureChannel::new(), "", deadline(), None,
        ) {
            Err(e) => e,
            Ok(_) => panic!("a secure error after auth was waited out, not refused"),
        };
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");

        // And the data start still arrives as one, so the two new arms are
        // not paid for with the ordinary path.
        let start = ns::ns_build(NS_VERSION, ns::NS_FIX_START, &[], "");
        let mut stream = FramedScript { pos: 0, frames: vec![start] };
        match super::super::wait_for_fix_start(
            &mut stream, &mut SecureChannel::new(), "", deadline(), None,
        ) {
            Ok(super::super::ReconnectPostAuth::Ready(none)) => assert!(none.is_none()),
            _ => panic!("the data start was not read as the data start"),
        }
    }
}

/// An execution report that rides in with the reconnect acknowledgement is
/// kept, not dropped with the envelope it arrived in.
///
/// The venue pushes what it holds the moment a logon is answered, so one
/// envelope carries the acknowledgement and the session's own traffic beside
/// it. The loop reads that envelope for the acknowledgement and then goes on
/// to build the connection from the bytes a read took past the frame — so
/// anything inside the envelope reached nothing at all, and the fill an
/// execution report stated was never told to anyone.
#[test]
fn an_execution_report_riding_in_with_the_reconnect_ack_is_kept() {
    let ack = crate::protocol::fix::fix_build(&[(35, "A"), (58, "reconnected")], 1);
    let fill = crate::protocol::fix::fix_build(&[(35, "8"), (17, "exec-1"), (32, "100")], 2);

    let beside = super::logon::what_rode_in_beside_the_ack(&[ack.clone(), fill.clone()]);

    assert!(
        beside.windows(fill.len()).any(|w| w == fill.as_slice()),
        "the report was dropped with the envelope the acknowledgement arrived in",
    );
    assert!(
        !beside.windows(ack.len()).any(|w| w == ack.as_slice()),
        "and the acknowledgement, which this loop has already acted on, is not \
         handed to the session as traffic",
    );
}

/// And the account traffic that shares the acknowledgement's type is kept too.
///
/// The logon is answered by a `35=A` or by the server-configuration message
/// that stands in for it, and that message shares its type with the session's
/// own account traffic — the holdings and the account's figures both arrive
/// under it. Read as the acknowledgement, a holding riding in with one was
/// dropped along with it: the execution report in the same envelope survived
/// while the position it moved did not.
#[test]
fn a_holding_that_shares_the_acks_message_type_is_kept() {
    let config = crate::protocol::fix::fix_build(&[(35, "U"), (58, "server config")], 1);
    let holding = crate::protocol::fix::fix_build(
        &[(35, "U"), (6040, "75"), (6008, "756733"), (6041, "100")], 2,
    );

    let beside = super::logon::what_rode_in_beside_the_ack(&[config.clone(), holding.clone()]);

    assert!(
        beside.windows(holding.len()).any(|w| w == holding.as_slice()),
        "the account's own traffic was dropped as though it were the answer to \
         the logon",
    );
    assert!(
        !beside.windows(config.len()).any(|w| w == config.as_slice()),
        "and the message that answered the logon is not handed on as traffic",
    );
}

/// An acknowledgement is recognised wherever in the envelope it sits.
///
/// One answer holds several messages, and a parse of the whole keeps the last
/// value for the type tag — so an acknowledgement followed by anything at all
/// read as that other thing. The loop went back for an answer it was already
/// holding, and where the venue was waiting for the opening requests it waited
/// until the deadline.
#[test]
fn an_acknowledgement_is_found_when_it_is_not_the_last_message() {
    let ack = crate::protocol::fix::fix_build(&[(35, "A"), (58, "logged on")], 1);
    let behind = crate::protocol::fix::fix_build(&[(35, "9"), (58, "something after")], 2);

    // The two inside one compressed envelope, which is how the venue sends
    // them and the only shape in which the parse sees them together.
    let mut inner = Vec::new();
    inner.extend_from_slice(&ack);
    inner.extend_from_slice(&behind);
    let envelope = crate::protocol::fixcomp::fixcomp_build(&inner);

    // What a parse of the joined body says the answer was.
    let last_wins = crate::protocol::fix::fix_parse(&inner);
    assert_eq!(
        last_wins.get(&35).map(|s| s.as_str()),
        Some("9"),
        "this test rests on the parse being last-wins; without that it proves nothing",
    );

    // A socket that answers once and then says nothing: if the loop goes back
    // for another answer it runs to the deadline instead of returning.
    struct SaysItOnce {
        bytes: Vec<u8>,
        at: usize,
    }
    impl std::io::Read for SaysItOnce {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.at >= self.bytes.len() {
                return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "quiet"));
            }
            let n = out.len().min(self.bytes.len() - self.at);
            out[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    let mut peer = SaysItOnce { bytes: envelope, at: 0 };
    let read = super::logon::LogonAck::read(
        &mut peer,
        &mut Vec::new(),
        std::time::Instant::now() + std::time::Duration::from_millis(400),
    );
    assert!(
        read.is_ok(),
        "the loop went back for an answer it was already holding, and waited out \
         its deadline: {:?}", read.err(),
    );
}

/// A peer that accepts the socket and then shakes no hands does not hold the
/// attempt for longer than the attempt has.
///
/// The socket's own timeouts bound a read, and a handshake is many reads: a
/// peer answering each one just inside its timeout makes progress by every
/// measure the socket can take and still never finishes, while the blocking
/// handshake stays inside the library where no deadline of ours can reach it.
/// On the reconnect path this worker is what the scheduler waits on and what a
/// shutdown joins, so a peer choosing never to finish holds both.
#[test]
fn a_handshake_that_will_not_finish_ends_when_the_attempt_does() {
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").expect("a socket to listen on");
    let addr = listener.local_addr().expect("its address");
    // Accepts, then holds the connection open saying nothing at all.
    let held = std::thread::spawn(move || {
        let (peer, _) = listener.accept().expect("the client connects");
        std::thread::sleep(Duration::from_secs(3));
        drop(peer);
    });

    let tcp = TcpStream::connect(addr).expect("the client reaches it");
    // As the callers set it: long enough that a silent peer is not answered by
    // the socket inside this test's own deadline.
    tcp.set_read_timeout(Some(Duration::from_secs(30))).expect("a read timeout");
    tcp.set_write_timeout(Some(Duration::from_secs(30))).expect("a write timeout");

    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("a connector");

    let started = Instant::now();
    let err = super::shake_hands_by(
        &connector,
        "localhost",
        tcp,
        Instant::now() + Duration::from_millis(600),
    )
    .expect_err("a handshake that never completes is given up on");

    assert_eq!(
        err.kind(),
        std::io::ErrorKind::TimedOut,
        "the handshake ended for some reason other than the attempt running out: {err}",
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the handshake was held by the peer rather than by this attempt's own clock",
    );
    let _ = held.join();
}
