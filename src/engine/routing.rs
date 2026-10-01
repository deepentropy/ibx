//! Farm routing table: which data farm serves a request.
//!
//! The server sends one table per primary farm after its logon: the market
//! data table on the market-data farm, the historical table on the
//! historical farm. Each row names an exchange (or an aggregate group), the
//! security types, the data kinds and the farm (host, port, name). A request
//! is routed by (exchange or aggregate group, security type, data kind,
//! listing exchange), first matching row in the reference's order.

use std::cmp::Ordering;

/// The connection a table arrived on, which gives the kind of its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    MarketData,
    Historical,
}

/// Data kind of a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DataType {
    Any,
    Top,
    Frz,
    Deep,
    DeepX,
    Deep2,
    AggDeep,
    Bar5Sec,
    NewsBody,
    ScanTop,
    ConAdj,
    DayChart,
    EODChart,
    StrtgScn,
    OI,
    DLDirect,
    TickByTick,
}

impl DataType {
    const ALL: [DataType; 17] = [
        DataType::Any, DataType::Top, DataType::Frz, DataType::Deep, DataType::Deep2, DataType::DeepX,
        DataType::AggDeep, DataType::Bar5Sec, DataType::ConAdj, DataType::ScanTop, DataType::DayChart,
        DataType::NewsBody, DataType::EODChart, DataType::StrtgScn, DataType::OI, DataType::DLDirect,
        DataType::TickByTick,
    ];

    /// The name used in the table.
    pub fn name(self) -> &'static str {
        match self {
            DataType::Any => "*",
            DataType::Top => "Top",
            DataType::Frz => "Frz",
            DataType::Deep => "Deep",
            DataType::DeepX => "DeepX",
            DataType::Deep2 => "Deep2",
            DataType::AggDeep => "AggDeep",
            DataType::Bar5Sec => "Bar5Sec",
            DataType::NewsBody => "NewsBody",
            DataType::ScanTop => "ScanTop",
            DataType::ConAdj => "ConAdj",
            DataType::DayChart => "DayChart",
            DataType::EODChart => "EODChart",
            DataType::StrtgScn => "StrtgScn",
            DataType::OI => "OI",
            DataType::DLDirect => "DLDirect",
            DataType::TickByTick => "TickByTick",
        }
    }

    /// The kind of a table name, case-insensitive; None when unknown.
    pub fn from_name(name: &str) -> Option<Self> {
        DataType::ALL.into_iter().find(|t| t.name().eq_ignore_ascii_case(name))
    }

    /// A depth kind, listed by reqMktDepthExchanges.
    pub fn is_depth(self) -> bool {
        matches!(self, DataType::Deep | DataType::Deep2 | DataType::DeepX | DataType::AggDeep)
    }
}

/// The farm a route leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub farm: String,
    pub host: String,
    pub port: u16,
}

/// One routing key, as the reference builds it from a row or a request.
#[derive(Debug, Clone)]
struct Key {
    /// The aggregate group when there is one, else the exchange.
    key: String,
    /// The exchange, the smart-routing name read as SMART.
    named: String,
    /// Listing exchange filter, `*` for any.
    listing: String,
    /// Security type (API name), `*` for any.
    sec_type: String,
    data_type: DataType,
    agg_group: i32,
}

/// Exchange as the reference keys it.
fn smart_name(exchange: &str) -> &str {
    if exchange == "BEST" { "SMART" } else { exchange }
}

impl Key {
    fn new(exchange: &str, agg_group: i32, sec_type: &str, data_type: DataType, listing: &str) -> Self {
        if exchange == "ANYEXCH" {
            return Key {
                key: "*".into(), named: "*".into(), listing: listing.into(),
                sec_type: sec_type.into(), data_type, agg_group,
            };
        }
        let key = if agg_group != -1 { agg_group.to_string() } else { smart_name(exchange).to_string() };
        Key {
            key: smart_name(&key).to_string(),
            named: smart_name(exchange).to_string(),
            listing: listing.into(),
            sec_type: sec_type.into(),
            data_type,
            agg_group,
        }
    }

    /// Same key: a later row with it replaces the earlier one.
    fn same(&self, other: &Key) -> bool {
        self.key == other.key && self.named == other.named && self.listing == other.listing
            && self.sec_type == other.sec_type && self.data_type == other.data_type
    }

    /// The reference's order of the keys of one data kind: by key, listing
    /// exchange, security type, then the whole key; `*` sorts last and
    /// names holding a `/` first.
    fn order(&self, other: &Key) -> Ordering {
        part_order(&self.key, &other.key)
            .then_with(|| part_order(&self.listing, &other.listing))
            .then_with(|| part_order(&self.sec_type, &other.sec_type))
            .then_with(|| part_order(self.data_type.name(), other.data_type.name()))
            .then_with(|| self.text().cmp(&other.text()))
    }

    fn text(&self) -> String {
        format!("exch={} named={} secType={} dataType={} listingExchange={}",
            self.key, self.named, self.sec_type, self.data_type.name(), self.listing)
    }

    /// Whether the row `row` serves the request key `self`.
    fn matches(&self, row: &Key) -> bool {
        if self.data_type != row.data_type {
            return false;
        }
        let sec_type = self.sec_type == row.sec_type || row.sec_type == "*" || self.sec_type == "*"
            || (self.sec_type.is_empty() && row.sec_type == "BOND");
        if !sec_type {
            return false;
        }
        let listing = self.listing == row.listing || row.listing == "*" || self.listing == "*";
        if !listing {
            return false;
        }
        if self.named == "SMART" && row.named == "SMART" {
            // A SMART request takes the row of its aggregate group, or a
            // row with no group.
            return self.key == row.key || row.key == "SMART";
        }
        self.named == row.named || row.named == "*" || self.named == "*"
    }
}

fn part_order(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    if a == "*" {
        return Ordering::Greater;
    }
    if b == "*" {
        return Ordering::Less;
    }
    match (a.contains('/'), b.contains('/')) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        _ => a.cmp(b),
    }
}

/// A row of reqMktDepthExchanges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepthExchange {
    pub exchange: String,
    pub sec_type: String,
    pub listing_exch: String,
    pub service_data_type: String,
    pub agg_group: i32,
}

/// A routing table, with the keys of each data kind in the reference's
/// order.
#[derive(Debug, Clone, Default)]
pub struct RoutingTable {
    keys: Vec<(DataType, Vec<(Key, usize)>)>,
    routes: Vec<Route>,
}

impl RoutingTable {
    /// Parse the rows of a table as the reference reads them: rows it does
    /// not support and unknown data kinds are skipped.
    pub fn parse(text: &str, kind: TableKind) -> Self {
        let mut table = RoutingTable::default();
        for row in text.split(';') {
            let row = row.trim();
            if row.is_empty() || row.starts_with("SMART") {
                continue;
            }
            let fields: Vec<&str> = row.split(',').map(str::trim).collect();
            let (exchange, sec_types, types, agg, listing, host, port, farm) = match fields[..] {
                [e, s, t, a, l, h, p, f] => (e, s, t, a, l, h, p, f),
                [e, s, t, a, h, p, f] => (e, s, t, a, "*", h, p, f),
                _ => {
                    log::debug!("Routing row of an unknown shape skipped: {}", row);
                    continue;
                }
            };
            let Ok(agg_group) = agg.parse::<i32>() else {
                log::debug!("Routing row with a bad aggregate group skipped: {}", row);
                continue;
            };
            let route = Route { farm: farm.to_string(), host: host.to_string(), port: port.parse().unwrap_or(0) };
            let idx = match table.routes.iter().position(|r| *r == route) {
                Some(i) => i,
                None => {
                    table.routes.push(route);
                    table.routes.len() - 1
                }
            };
            let mut data_types = Vec::new();
            for t in types.split('|') {
                let t = t.trim();
                let dt = if t.eq_ignore_ascii_case("EOD") {
                    Some(DataType::EODChart)
                } else if t.eq_ignore_ascii_case("News") {
                    Some(DataType::Any)
                } else {
                    DataType::from_name(t)
                };
                match dt {
                    Some(dt) => data_types.push(dt),
                    None => log::debug!("Unexpected data type {} in {}", t, row),
                }
            }
            for sec_type in sec_types.split('|') {
                let sec_type = match sec_type.trim() {
                    "" => {
                        log::debug!("Unexpected sec type in {}", row);
                        continue;
                    }
                    "COMB" => "BAG",
                    other => other,
                };
                for &dt in &data_types {
                    // The keys of a kind, as the reference builds them for
                    // each table kind.
                    let keys: Vec<(DataType, &str)> = match (kind, dt) {
                        (TableKind::MarketData, DataType::Any) => vec![(DataType::Top, listing)],
                        (TableKind::MarketData, dt) => vec![(dt, listing)],
                        (TableKind::Historical, DataType::Any) =>
                            vec![(DataType::ScanTop, "*"), (DataType::DayChart, "*"), (DataType::EODChart, "*")],
                        (TableKind::Historical, DataType::Top) => vec![(DataType::DayChart, "*")],
                        (TableKind::Historical, dt) => vec![(dt, "*")],
                    };
                    for (dt, listing) in keys {
                        table.insert(Key::new(exchange, agg_group, sec_type, dt, listing), idx);
                    }
                }
            }
        }
        table
    }

    fn insert(&mut self, key: Key, route: usize) {
        let pos = match self.keys.iter().position(|(dt, _)| *dt == key.data_type) {
            Some(p) => p,
            None => {
                self.keys.push((key.data_type, Vec::new()));
                self.keys.len() - 1
            }
        };
        let list = &mut self.keys[pos].1;
        if let Some(existing) = list.iter_mut().find(|(k, _)| k.same(&key)) {
            *existing = (key, route);
            return;
        }
        let at = list.partition_point(|(k, _)| k.order(&key) == Ordering::Less);
        list.insert(at, (key, route));
    }

    /// Whether the table has no row.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The farm of a request: the routing exchange as the request writes
    /// it, the contract's aggregate group for a SMART request (-1 for
    /// none), the security type (API name), the data kind and the listing
    /// exchange (`*` for any). None when no row serves it.
    pub fn lookup(&self, exchange: &str, agg_group: i32, sec_type: &str, data_type: DataType, listing: &str) -> Option<&Route> {
        let sec_type = if sec_type == "COMB" { "BAG" } else { sec_type };
        let listing = if listing.is_empty() { "*" } else { listing };
        let want = Key::new(exchange, agg_group, sec_type, data_type, listing);
        let (_, list) = self.keys.iter().find(|(dt, _)| *dt == data_type)?;
        list.iter().find(|(row, _)| want.matches(row)).map(|(_, idx)| &self.routes[*idx])
    }

    /// Every route of a depth kind, as reqMktDepthExchanges lists them.
    pub fn depth_exchanges(&self) -> Vec<DepthExchange> {
        let mut out = Vec::new();
        for (dt, list) in &self.keys {
            if !dt.is_depth() {
                continue;
            }
            for (key, _) in list {
                out.push(DepthExchange {
                    exchange: key.named.clone(),
                    sec_type: key.sec_type.clone(),
                    listing_exch: if key.listing == "*" { String::new() } else { key.listing.clone() },
                    service_data_type: dt.name().to_string(),
                    agg_group: key.agg_group,
                });
            }
        }
        out
    }

    /// The farms of the table.
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MD: &str = "ADX,STK,Frz,-1,*,zdc1.example,4000,eufarm;\
        BEST,STK,AggDeep,1,OTCBB,cdc1.example,4000,usfarm;\
        BEST,STK,Top,1,*,cdc1.example,4000,usfarm;\
        BEST,STK,Top,3,*,zdc1.example,4000,eufarm;\
        BEST,CMDTY,Top|Deep,14,*,ndc1.example,4000,cashfarm;\
        CME,FUT,Top|Deep,-1,*,cdc1.example,4000,usfuture;\
        IDEALPRO,CASH,Top|Deep,4,*,ndc1.example,4000,cashfarm;\
        NASDAQ,STK,Top|Deep2|Deep,-1,*,cdc1.example,4000,usfarm;\
        MEMX,STK,Top,-1,*,cdc1.example,4000,usfarm;\
        IEX,STK,Top|Deep,-1,*,cdc1.example,4000,usfarm;\
        SEHK,STK,Top|DeepX|Deep,-1,*,hdc1.example,4000,hfarm;\
        NEWS,*,*,-1,*,ndc1.example,4000,usfarm.nj;\
        SMART,STK,Top,1,*,cdc1.example,4000,bogus;\
        OLD,STK,Top,-1;\
        *,STK,Top,-1,*,ndc1.example,4000,usfarm.nj";

    const HMDS: &str = "BEST,STK,DayChart|EODChart|Bar5Sec|ConAdj,3,*,zdc1.example,4000,euhmds;\
        BEST,STK,DayChart|EODChart|ScanTop|Bar5Sec|ConAdj,1,*,cdc1.example,4000,ushmds;\
        BEST,STK,TickByTick,1,*,cdc1.example,4000,ushmds;\
        CME,FUT,DayChart|EODChart|Bar5Sec|ScanTop,-1,*,cdc1.example,4000,ushmds;\
        CME,FUT,TickByTick,-1,*,cdc1.example,4000,ushmds;\
        IDEALPRO,CASH,DayChart|EODChart|Bar5Sec,4,*,ndc1.example,4000,cashhmds;\
        IDEALPRO,CASH,TickByTick,4,*,ndc1.example,4000,cashhmds;\
        RTRSFND,*,*,-1,*,ndc1.example,4000,fundfarm;\
        *,STK,ConAdj,-1,*,cdc1.example,4000,ushmds";

    fn farm(t: &RoutingTable, e: &str, agg: i32, st: &str, dt: DataType, l: &str) -> Option<String> {
        t.lookup(e, agg, st, dt, l).map(|r| r.farm.clone())
    }

    // A SMART stock takes the row of its aggregate group; a directed
    // contract the row of its exchange (captured: a CME future on
    // usfuture, EUR.USD on cashfarm).
    #[test]
    fn top_rows_by_group_or_exchange() {
        let t = RoutingTable::parse(MD, TableKind::MarketData);
        assert_eq!(farm(&t, "BEST", 1, "STK", DataType::Top, "NASDAQ").as_deref(), Some("usfarm"));
        assert_eq!(farm(&t, "BEST", 3, "STK", DataType::Top, "IBIS").as_deref(), Some("eufarm"));
        assert_eq!(farm(&t, "CME", -1, "FUT", DataType::Top, "*").as_deref(), Some("usfuture"));
        assert_eq!(farm(&t, "IDEALPRO", -1, "CASH", DataType::Top, "*").as_deref(), Some("cashfarm"));
        assert_eq!(farm(&t, "NASDAQ", -1, "STK", DataType::Top, "NASDAQ").as_deref(), Some("usfarm"));
        // An exchange with no row of its own takes the `*` row, last in order.
        assert_eq!(farm(&t, "ZZZ", -1, "STK", DataType::Top, "*").as_deref(), Some("usfarm.nj"));
        // No row for the security type.
        assert_eq!(farm(&t, "CME", -1, "OPT", DataType::Top, "*"), None);
        // A SMART request with a group no row has, and no group-less SMART
        // row: the `*` row.
        assert_eq!(farm(&t, "BEST", 99, "STK", DataType::Top, "*").as_deref(), Some("usfarm.nj"));
        // A SMART row is skipped.
        assert!(t.routes().iter().all(|r| r.farm != "bogus"));
    }

    // Depth kinds have their own rows; the listing exchange filter only
    // lets its own listing through.
    #[test]
    fn depth_rows_and_listing_filter() {
        let t = RoutingTable::parse(MD, TableKind::MarketData);
        assert_eq!(farm(&t, "NASDAQ", -1, "STK", DataType::Deep, "*").as_deref(), Some("usfarm"));
        assert_eq!(farm(&t, "NASDAQ", -1, "STK", DataType::Deep2, "*").as_deref(), Some("usfarm"));
        assert_eq!(farm(&t, "MEMX", -1, "STK", DataType::Deep, "*"), None);
        assert_eq!(farm(&t, "BEST", 1, "STK", DataType::AggDeep, "OTCBB").as_deref(), Some("usfarm"));
        assert_eq!(farm(&t, "BEST", 1, "STK", DataType::AggDeep, "NASDAQ"), None);
        assert_eq!(farm(&t, "BEST", 1, "STK", DataType::Deep, "NASDAQ"), None);
    }

    // A `*` kind of a market data row is a Top row; of a historical row it
    // is the chart and scanner kinds. Top in a historical row is DayChart.
    #[test]
    fn any_kind_expands_by_table() {
        let md = RoutingTable::parse(MD, TableKind::MarketData);
        assert_eq!(farm(&md, "NEWS", -1, "NEWS", DataType::Top, "*").as_deref(), Some("usfarm.nj"));
        let h = RoutingTable::parse(HMDS, TableKind::Historical);
        assert_eq!(farm(&h, "RTRSFND", -1, "STK", DataType::DayChart, "*").as_deref(), Some("fundfarm"));
        assert_eq!(farm(&h, "RTRSFND", -1, "STK", DataType::EODChart, "*").as_deref(), Some("fundfarm"));
        assert_eq!(farm(&h, "RTRSFND", -1, "STK", DataType::TickByTick, "*"), None);
        assert_eq!(farm(&h, "BEST", 1, "STK", DataType::TickByTick, "*").as_deref(), Some("ushmds"));
        assert_eq!(farm(&h, "BEST", 3, "STK", DataType::DayChart, "*").as_deref(), Some("euhmds"));
        assert_eq!(farm(&h, "IDEALPRO", -1, "CASH", DataType::TickByTick, "*").as_deref(), Some("cashhmds"));
        assert_eq!(farm(&h, "CME", -1, "FUT", DataType::TickByTick, "*").as_deref(), Some("ushmds"));
        let t = RoutingTable::parse("X,STK,Top,-1,*,h,4000,f", TableKind::Historical);
        assert_eq!(farm(&t, "X", -1, "STK", DataType::DayChart, "*").as_deref(), Some("f"));
    }

    // Seven-field rows have no listing filter.
    #[test]
    fn seven_field_rows() {
        let t = RoutingTable::parse("CME,FUT,Top,-1,cdc1.example,4000,usfuture", TableKind::MarketData);
        let r = t.lookup("CME", -1, "FUT", DataType::Top, "GLOBEX").unwrap();
        assert_eq!((r.farm.as_str(), r.host.as_str(), r.port), ("usfuture", "cdc1.example", 4000));
    }

    // reqMktDepthExchanges: every depth route, the smart-routing row shown
    // as SMART, no listing as empty; a top-only exchange is not listed.
    #[test]
    fn depth_exchanges_from_depth_rows() {
        let t = RoutingTable::parse(MD, TableKind::MarketData);
        let rows = t.depth_exchanges();
        assert!(rows.iter().all(|r| r.exchange != "MEMX"));
        assert!(rows.contains(&DepthExchange {
            exchange: "SMART".into(), sec_type: "STK".into(), listing_exch: "OTCBB".into(),
            service_data_type: "AggDeep".into(), agg_group: 1,
        }));
        assert!(rows.contains(&DepthExchange {
            exchange: "NASDAQ".into(), sec_type: "STK".into(), listing_exch: String::new(),
            service_data_type: "Deep2".into(), agg_group: -1,
        }));
        assert!(rows.contains(&DepthExchange {
            exchange: "SEHK".into(), sec_type: "STK".into(), listing_exch: String::new(),
            service_data_type: "DeepX".into(), agg_group: -1,
        }));
        assert_eq!(rows.iter().filter(|r| r.service_data_type == "Deep").count(), 6);
    }
}

/// The rows of a routing table message, None when `msg` is not one.
pub fn table_text(msg: &[u8]) -> Option<String> {
    let start = msg.windows(5).position(|w| w == b"35=T\x01")? + 5;
    let mut rest = &msg[start..];
    if rest.starts_with(b"6556=") {
        let end = rest.iter().position(|&b| b == 0x01)?;
        rest = &rest[end + 1..];
    }
    let end = rest.iter().position(|&b| b == 0x01).unwrap_or(rest.len());
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

#[cfg(test)]
mod table_text_tests {
    #[test]
    fn rows_of_a_table_message() {
        let msg = b"8=O\x019=60\x0135=T\x016556=2\x01CME,FUT,Top,-1,*,h,4000,usfuture;X,STK,Top,-1,*,h,4000,f\x018349=AB\x01";
        assert_eq!(super::table_text(msg).as_deref(), Some("CME,FUT,Top,-1,*,h,4000,usfuture;X,STK,Top,-1,*,h,4000,f"));
        assert_eq!(super::table_text(b"8=O\x019=5\x0135=P\x01xx"), None);
    }
}
