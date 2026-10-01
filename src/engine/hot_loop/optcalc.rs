//! Option calculations on the auth connection (ibx#442): the option
//! definition by conId, the rate curve and the dividends as reference-data
//! queries, then the local model; one answer tick per request.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::bridge::SharedState;
use crate::config::chrono_free_timestamp;
use crate::control::optcalc::{self, inputs, CalcKind, OptionTerms, Right, Style};
use crate::protocol::connection::Connection;
use crate::protocol::fix;
use crate::types::ReqId;

use super::HeartbeatState;

/// Request numbers of the definition lookups of option calculations: a
/// range of their own, below the other internal lookups.
pub(crate) const OPTCALC_LOOKUP_FIRST_ID: u32 = 0xC000_0000;
pub(crate) const OPTCALC_LOOKUP_IDS: u32 = 0x1000_0000;
/// A definition lookup with no answer in this time ends its calculations.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Request id prefix of the reference-data queries.
const XML_QUERY_PREFIX: &str = "SubMsgXMLQuery";

const IV_NOT_OPTION: (i64, &str) = (2114, "Calculation of Implied Volatility supported for option securities only");
const PRICE_NOT_OPTION: (i64, &str) = (2116, "Calculation of Option Price supported for option securities only");

#[derive(Debug, Clone)]
struct PendingCalc {
    req_id: ReqId,
    con_id: i64,
    kind: CalcKind,
    under_price: f64,
    /// Start of the answer window, once the option is known.
    started: Option<Instant>,
}

#[derive(Debug, Clone)]
enum Known<T> {
    Asked(Instant),
    Ready(T),
    Missing,
}

#[derive(Debug, Clone)]
enum XmlTarget {
    Curve(String),
    Dividends(i64),
}

/// State of the option calculations.
#[derive(Debug)]
pub(crate) struct OptCalc {
    clock: Option<inputs::ModelClock>,
    local_zone: String,
    calcs: Vec<PendingCalc>,
    terms: HashMap<i64, Known<OptionTerms>>,
    lookups: HashMap<u32, i64>,
    next_lookup: u32,
    curves: HashMap<String, Known<inputs::RateCurve>>,
    dividends: HashMap<i64, Known<Vec<inputs::Dividend>>>,
    xml_queries: HashMap<String, XmlTarget>,
    next_xml: u32,
}

impl Default for OptCalc {
    fn default() -> Self {
        OptCalc {
            clock: None,
            local_zone: inputs::local_zone_name(),
            calcs: Vec::new(),
            terms: HashMap::new(),
            lookups: HashMap::new(),
            next_lookup: 0,
            curves: HashMap::new(),
            dividends: HashMap::new(),
            xml_queries: HashMap::new(),
            next_xml: 1,
        }
    }
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// Terms of an option from its definition.
pub(crate) fn terms_of(def: &crate::control::contracts::ContractDefinition) -> OptionTerms {
    use crate::control::contracts::OptionRight;
    let sec_type = match def.wire_sec_type.as_str() {
        "CS" | "" => def.sec_type.to_api_str().to_string(),
        other => other.to_string(),
    };
    OptionTerms {
        con_id: def.con_id,
        sec_type,
        currency: def.currency.clone(),
        right: match def.right {
            Some(OptionRight::Call) => Some(Right::Call),
            Some(OptionRight::Put) => Some(Right::Put),
            None => None,
        },
        // Unknown exercise style is European, as the reference.
        style: if def.exercise_style == 1 { Style::American } else { Style::European },
        strike: def.strike,
        last_trade_date: def.last_trade_date.clone(),
        last_trade_time: def.last_trade_time.clone(),
        time_zone: def.time_zone_id.clone().unwrap_or_default(),
        under_con_id: def.under_con_id,
        under_sec_type: def.under_sec_type.clone(),
    }
}

impl OptCalc {
    fn clock_ms(&mut self, now: i64) -> i64 {
        let clock = *self.clock.get_or_insert(inputs::ModelClock { start_ms: now });
        clock.at(now)
    }

    /// A new calculation. Its definition and inputs are asked when not known.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        &mut self, req_id: ReqId, con_id: i64, kind: CalcKind, under_price: f64,
        conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState,
    ) {
        // The model starts with the first calculation; its clock with it.
        let _ = self.clock_ms(now_ms());
        self.calcs.push(PendingCalc { req_id, con_id, kind, under_price, started: None });
        if !self.terms.contains_key(&con_id) {
            let number = OPTCALC_LOOKUP_FIRST_ID + self.next_lookup % OPTCALC_LOOKUP_IDS;
            self.next_lookup = self.next_lookup.wrapping_add(1);
            self.lookups.insert(number, con_id);
            self.terms.insert(con_id, Known::Asked(Instant::now()));
            if let Some(c) = conn.as_mut() {
                let ts = chrono_free_timestamp();
                let number_str = number.to_string();
                let con_id_str = con_id.to_string();
                let _ = c.send_fix(&[
                    (fix::TAG_MSG_TYPE, "c"),
                    (fix::TAG_SENDING_TIME, &ts),
                    (crate::control::contracts::TAG_SECURITY_REQ_ID, &number_str),
                    (crate::control::contracts::TAG_SECURITY_REQ_TYPE, "2"),
                    (crate::control::contracts::TAG_IB_CON_ID, &con_id_str),
                    (crate::control::contracts::TAG_IB_SOURCE, "Socket"),
                ]);
                hb.last_ccp_sent = Instant::now();
            }
            log::info!("Option calculation req_id={}: definition of conId {} asked ({})", req_id, con_id, number);
        }
        self.progress(conn, hb, shared);
    }

    /// The definition answer of a lookup of this module; false when the
    /// reply is for another lookup.
    pub(crate) fn secdef_reply(
        &mut self, request_id: &str, msg: &[u8],
        conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState,
    ) -> bool {
        let Some(number) = crate::control::contracts::secdef_request_number(request_id) else { return false };
        let Ok(number) = u32::try_from(number) else { return false };
        let Some(con_id) = self.lookups.remove(&number) else { return false };
        let records = crate::control::contracts::parse_secdef_records(msg).unwrap_or_default();
        match records.iter().find(|d| d.con_id == con_id).or(records.first()) {
            Some(def) => {
                self.terms.insert(con_id, Known::Ready(terms_of(def)));
            }
            None => {
                self.terms.insert(con_id, Known::Missing);
            }
        }
        self.progress(conn, hb, shared);
        true
    }

    /// A reference-data answer (`320` id, `6118` XML); false when the id
    /// is not one of this module's queries.
    pub(crate) fn xml_reply(
        &mut self, query_id: &str, xml: &str,
        conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState,
    ) -> bool {
        let Some(target) = self.xml_queries.remove(query_id) else { return false };
        let items = inputs::parse_reference_xml(xml);
        match target {
            XmlTarget::Curve(currency) => {
                // The curve is dated at the model date of its arrival.
                let clock = self.clock_ms(now_ms());
                let base = inputs::midnight_ms(inputs::date_of_ms(clock, inputs::MODEL_TZ), inputs::MODEL_TZ);
                let curve = inputs::RateCurve::from_items(&items, base);
                log::info!("Option model: interest rates of {} ({} points)", currency, curve.points.len());
                self.curves.insert(currency, Known::Ready(curve));
            }
            XmlTarget::Dividends(con_id) => {
                let divs = inputs::dividends_from_items(&items);
                log::info!("Option model: dividends of conId {} ({} payments)", con_id, divs.len());
                self.dividends.insert(con_id, Known::Ready(divs));
            }
        }
        self.progress(conn, hb, shared);
        true
    }

    fn send_xml_query(&mut self, query: &str, target: XmlTarget, conn: &mut Option<Connection>, hb: &mut HeartbeatState) {
        let id = format!("{}{}", XML_QUERY_PREFIX, self.next_xml);
        self.next_xml = self.next_xml.wrapping_add(1);
        if let Some(c) = conn.as_mut() {
            let ts = chrono_free_timestamp();
            let _ = c.send_fix(&[
                (fix::TAG_MSG_TYPE, "U"),
                (fix::TAG_SENDING_TIME, &ts),
                (6040, "27"),
                (320, &id),
                (58, query),
            ]);
            hb.last_ccp_sent = Instant::now();
        }
        log::info!("Option model: reference-data query {} '{}'", id, query);
        self.xml_queries.insert(id, target);
    }

    /// Ask the missing inputs of the known options, answer the ready
    /// calculations and end the ones out of time.
    pub(crate) fn progress(&mut self, conn: &mut Option<Connection>, hb: &mut HeartbeatState, shared: &SharedState) {
        if self.calcs.is_empty() {
            return;
        }
        let now = Instant::now();
        let clock = self.clock_ms(now_ms());
        let calcs = std::mem::take(&mut self.calcs);
        let mut keep = Vec::with_capacity(calcs.len());
        for mut c in calcs {
            let terms = match self.terms.get(&c.con_id) {
                Some(Known::Ready(t)) => t.clone(),
                Some(Known::Asked(at)) => {
                    if now.duration_since(*at) >= LOOKUP_TIMEOUT {
                        self.terms.remove(&c.con_id);
                        shared.orders.push_order_error(c.req_id, 200, super::ccp::NO_SECURITY_DEFINITION.to_string());
                    } else {
                        keep.push(c);
                    }
                    continue;
                }
                _ => {
                    shared.orders.push_order_error(c.req_id, 200, super::ccp::NO_SECURITY_DEFINITION.to_string());
                    continue;
                }
            };
            if !terms.is_option() {
                let (code, text) = match c.kind {
                    CalcKind::ImpliedVol { .. } => IV_NOT_OPTION,
                    CalcKind::Price { .. } => PRICE_NOT_OPTION,
                };
                shared.orders.push_order_error(c.req_id, code, text.to_string());
                continue;
            }
            let started = *c.started.get_or_insert(now);
            // The inputs, asked once per currency and per underlying.
            if !self.curves.contains_key(&terms.currency) {
                self.curves.insert(terms.currency.clone(), Known::Asked(now));
                self.send_xml_query(&terms.rate_query(), XmlTarget::Curve(terms.currency.clone()), conn, hb);
            }
            if let std::collections::hash_map::Entry::Vacant(e) = self.dividends.entry(terms.under_con_id) {
                e.insert(Known::Asked(now));
                self.send_xml_query(&terms.dividend_query(), XmlTarget::Dividends(terms.under_con_id), conn, hb);
            }
            let ready = match (self.curves.get(&terms.currency), self.dividends.get(&terms.under_con_id)) {
                (Some(Known::Ready(curve)), Some(Known::Ready(divs))) => Some((curve, divs)),
                _ => None,
            };
            let elapsed = now.duration_since(started);
            match c.kind {
                CalcKind::ImpliedVol { option_price } => {
                    if let Some((curve, divs)) = ready {
                        let inp = optcalc::ModelInputs { curve, dividends: divs, clock_ms: clock, local_zone: &self.local_zone };
                        match optcalc::implied_vol_answer(c.req_id, &terms, option_price, c.under_price, &inp) {
                            Some(answer) => shared.reference.push_option_computation(answer),
                            // No volatility for this price: no answer, as the reference.
                            None => log::info!("Option calculation req_id={}: no implied volatility", c.req_id),
                        }
                    } else if elapsed < Duration::from_millis(optcalc::IV_WINDOW_MS) {
                        keep.push(c);
                    } else {
                        log::info!("Option calculation req_id={}: model not ready in the window, no answer", c.req_id);
                    }
                }
                CalcKind::Price { volatility } => {
                    let answer = ready.and_then(|(curve, divs)| {
                        let inp = optcalc::ModelInputs { curve, dividends: divs, clock_ms: clock, local_zone: &self.local_zone };
                        optcalc::price_answer(c.req_id, &terms, volatility, c.under_price, &inp)
                    });
                    if let Some(answer) = answer {
                        shared.reference.push_option_computation(answer);
                    } else if elapsed >= Duration::from_millis(optcalc::PRICE_TIMEOUT_MS) {
                        shared.reference.push_option_computation(
                            optcalc::price_timeout_answer(c.req_id, volatility, c.under_price));
                    } else {
                        keep.push(c);
                    }
                }
            }
        }
        // Calculations started while answering stay too.
        keep.append(&mut self.calcs);
        self.calcs = keep;
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.calcs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> SharedState {
        SharedState::new()
    }

    fn option_def(right: crate::control::contracts::OptionRight) -> crate::control::contracts::ContractDefinition {
        crate::control::contracts::ContractDefinition {
            con_id: 848313396,
            sec_type: crate::control::contracts::SecurityType::Option,
            wire_sec_type: "OPT".into(),
            currency: "USD".into(),
            right: Some(right),
            strike: 255.0,
            last_trade_date: "20260306".into(),
            exercise_style: 1,
            under_con_id: 265598,
            under_sec_type: "STK".into(),
            ..Default::default()
        }
    }

    #[test]
    fn terms_come_from_the_definition() {
        let t = terms_of(&option_def(crate::control::contracts::OptionRight::Put));
        assert_eq!((t.sec_type.as_str(), t.right, t.style, t.under_con_id), ("OPT", Some(Right::Put), Style::American, 265598));
        let mut d = option_def(crate::control::contracts::OptionRight::Call);
        d.exercise_style = 0;
        assert_eq!(terms_of(&d).style, Style::European);
        d.wire_sec_type = "FOP".into();
        d.sec_type = crate::control::contracts::SecurityType::Other;
        assert_eq!(terms_of(&d).sec_type, "FOP");
        d.wire_sec_type = "CS".into();
        d.sec_type = crate::control::contracts::SecurityType::Stock;
        assert!(!terms_of(&d).is_option());
    }

    fn ready_state(kind: CalcKind) -> (OptCalc, SharedState) {
        let mut oc = OptCalc::default();
        let shared = shared();
        oc.terms.insert(848313396, Known::Ready(terms_of(&option_def(crate::control::contracts::OptionRight::Call))));
        oc.calcs.push(PendingCalc { req_id: 7, con_id: 848313396, kind, under_price: 250.0, started: None });
        (oc, shared)
    }

    #[test]
    fn inputs_are_asked_once_then_the_answer_follows() {
        let (mut oc, shared) = ready_state(CalcKind::Price { volatility: 0.3 });
        let (mut conn, mut hb) = (None, HeartbeatState::new());
        oc.progress(&mut conn, &mut hb, &shared);
        assert_eq!(oc.xml_queries.len(), 2);
        assert_eq!(oc.pending(), 1);
        oc.progress(&mut conn, &mut hb, &shared);
        assert_eq!(oc.xml_queries.len(), 2, "asked once");
        let ids: Vec<(String, XmlTarget)> = oc.xml_queries.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (id, target) in ids {
            let xml = match target {
                XmlTarget::Curve(_) => "<dividends><div><date>20200101</date><curr>USD</curr><amt>3.9</amt></div></dividends>",
                XmlTarget::Dividends(_) => "<dividends policy=\"regular\"></dividends>",
            };
            assert!(oc.xml_reply(&id, xml, &mut conn, &mut hb, &shared));
        }
        let answers = shared.reference.drain_option_computations();
        assert_eq!(answers.len(), 1);
        assert_eq!((answers[0].req_id, answers[0].tick_type, answers[0].implied_vol), (7, 53, 0.3));
        assert_eq!(oc.pending(), 0);
        assert!(!oc.xml_reply("SubMsgXMLQuery999", "", &mut conn, &mut hb, &shared));
    }

    #[test]
    fn a_price_out_of_time_has_no_price() {
        let (mut oc, shared) = ready_state(CalcKind::Price { volatility: 0.3 });
        oc.calcs[0].started = Some(Instant::now() - Duration::from_millis(optcalc::PRICE_TIMEOUT_MS));
        let (mut conn, mut hb) = (None, HeartbeatState::new());
        oc.progress(&mut conn, &mut hb, &shared);
        let answers = shared.reference.drain_option_computations();
        assert_eq!(answers.len(), 1);
        assert_eq!((answers[0].opt_price, answers[0].delta, answers[0].implied_vol), (optcalc::NOT_COMPUTED, optcalc::NOT_COMPUTED, 0.3));
    }

    #[test]
    fn an_implied_volatility_out_of_time_has_no_answer() {
        let (mut oc, shared) = ready_state(CalcKind::ImpliedVol { option_price: 5.0 });
        oc.calcs[0].started = Some(Instant::now() - Duration::from_millis(optcalc::IV_WINDOW_MS));
        let (mut conn, mut hb) = (None, HeartbeatState::new());
        oc.progress(&mut conn, &mut hb, &shared);
        assert!(shared.reference.drain_option_computations().is_empty());
        assert_eq!(oc.pending(), 0);
    }

    #[test]
    fn local_refusals_before_any_lookup() {
        use crate::client_core::ClientCore;
        let c = |sec: &str, con_id: i64| crate::api::types::Contract {
            con_id, symbol: "AAPL".into(), sec_type: sec.into(), exchange: "SMART".into(), ..Default::default()
        };
        let text = |r: Option<(i64, String)>| r.map(|(code, t)| format!("{code} {t}"));
        let no_exch = crate::api::types::Contract { exchange: String::new(), ..c("OPT", 1) };
        assert_eq!(text(ClientCore::option_calc_refusal(&no_exch, true, None)).unwrap(),
            "321 Error validating request.-'bJ' : cause - Please enter exchange");
        assert_eq!(text(ClientCore::option_calc_refusal(&c("STK", 0), false, None)).unwrap(),
            "321 Error validating request.-'bI' : cause - Calculation of Implied Volatility supported for option securities only");
        assert_eq!(text(ClientCore::option_calc_refusal(&c("STK", 0), true, None)).unwrap(),
            "321 Error validating request.-'bJ' : cause - Calculation of Option Price supported for option securities only");
        // With a conId, a stock passes the local checks; the lookup answers 2114 / 2116.
        assert!(ClientCore::option_calc_refusal(&c("STK", 265598), true, None).is_none());
        assert_eq!(text(ClientCore::option_calc_refusal(&c("OPT", 0), true, None)).unwrap(),
            "321 Error validating request.-'bJ' : cause - When the local symbol field is empty, please fill all option fields (right, strike, expiry)");
        let full = crate::api::types::Contract { last_trade_date_or_contract_month: "20260306".into(), right: "C".into(), ..c("OPT", 0) };
        let features = vec!["ZEROSTRKOPT".to_string()];
        assert!(ClientCore::option_calc_refusal(&full, true, Some(&features)).is_none());
        assert!(ClientCore::option_calc_refusal(&full, true, None).is_some(), "strike 0 needs the account feature");
        assert_eq!(text(ClientCore::option_calc_refusal(&c("FOP", 0), true, None)).unwrap(),
            "321 Error validating request.-'bJ' : cause - Please enter a local symbol or an expiry");
        // The implied volatility request has only the first two checks.
        assert!(ClientCore::option_calc_refusal(&c("OPT", 0), false, None).is_none());
    }

    #[test]
    fn a_stock_gets_the_option_only_error() {
        let mut oc = OptCalc::default();
        let shared = shared();
        let mut stock = option_def(crate::control::contracts::OptionRight::Call);
        stock.wire_sec_type = "CS".into();
        stock.sec_type = crate::control::contracts::SecurityType::Stock;
        oc.terms.insert(265598, Known::Ready(terms_of(&stock)));
        oc.calcs.push(PendingCalc { req_id: 3, con_id: 265598, kind: CalcKind::ImpliedVol { option_price: 1.0 }, under_price: 1.0, started: None });
        oc.calcs.push(PendingCalc { req_id: 4, con_id: 265598, kind: CalcKind::Price { volatility: 0.2 }, under_price: 1.0, started: None });
        let (mut conn, mut hb) = (None, HeartbeatState::new());
        oc.progress(&mut conn, &mut hb, &shared);
        let errors = shared.orders.drain_order_errors();
        assert!(errors.iter().any(|e| e.0 == 3 && e.1 == 2114));
        assert!(errors.iter().any(|e| e.0 == 4 && e.1 == 2116));
    }
}
