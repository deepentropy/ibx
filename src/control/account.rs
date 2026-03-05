//! Account and position synchronization from CCP (8=O messages).
//!
//! Handles account updates (balance, margin, buying power) and position
//! tracking from the CCP connection. Data flows to the hot loop via SPSC.

use std::collections::HashMap;

/// Account summary fields from CCP.
#[derive(Debug, Clone, Default)]
pub struct AccountSummary {
    pub account_id: String,
    pub net_liquidation: f64,
    pub total_cash: f64,
    pub buying_power: f64,
    pub gross_position_value: f64,
    pub maintenance_margin: f64,
    pub available_funds: f64,
    pub excess_liquidity: f64,
    pub currency: String,
}

/// A position update from CCP.
#[derive(Debug, Clone)]
pub struct PositionUpdate {
    pub account_id: String,
    pub con_id: u32,
    pub symbol: String,
    pub position: f64,
    pub avg_cost: f64,
    pub market_value: f64,
}

/// Commands that can be sent from control plane to hot loop.
#[derive(Debug)]
pub enum AccountCommand {
    /// Account summary updated.
    SummaryUpdate(AccountSummary),
    /// Position changed.
    PositionUpdate(PositionUpdate),
}

/// Parse account value from CCP 8=O tag-value pairs.
pub fn parse_account_value(tag: &str, value: &str, summary: &mut AccountSummary) {
    match tag {
        "AccountCode" => summary.account_id = value.to_string(),
        "NetLiquidation" => summary.net_liquidation = value.parse().unwrap_or(0.0),
        "TotalCashValue" => summary.total_cash = value.parse().unwrap_or(0.0),
        "BuyingPower" => summary.buying_power = value.parse().unwrap_or(0.0),
        "GrossPositionValue" => summary.gross_position_value = value.parse().unwrap_or(0.0),
        "MaintMarginReq" => summary.maintenance_margin = value.parse().unwrap_or(0.0),
        "AvailableFunds" => summary.available_funds = value.parse().unwrap_or(0.0),
        "ExcessLiquidity" => summary.excess_liquidity = value.parse().unwrap_or(0.0),
        "Currency" => summary.currency = value.to_string(),
        _ => {}
    }
}

/// Track all positions by conId.
#[derive(Debug, Default)]
pub struct PositionTracker {
    positions: HashMap<u32, PositionUpdate>,
}

impl PositionTracker {
    pub fn update(&mut self, pos: PositionUpdate) {
        self.positions.insert(pos.con_id, pos);
    }

    pub fn get(&self, con_id: u32) -> Option<&PositionUpdate> {
        self.positions.get(&con_id)
    }

    pub fn all(&self) -> impl Iterator<Item = &PositionUpdate> {
        self.positions.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Existing tests (preserved) ---

    #[test]
    fn parse_account_values() {
        let mut summary = AccountSummary::default();
        parse_account_value("NetLiquidation", "100000.50", &mut summary);
        parse_account_value("BuyingPower", "200000.00", &mut summary);
        parse_account_value("AccountCode", "DU12345", &mut summary);
        assert_eq!(summary.net_liquidation, 100000.50);
        assert_eq!(summary.buying_power, 200000.0);
        assert_eq!(summary.account_id, "DU12345");
    }

    #[test]
    fn position_tracker() {
        let mut tracker = PositionTracker::default();
        tracker.update(PositionUpdate {
            account_id: "DU12345".to_string(),
            con_id: 265598,
            symbol: "AAPL".to_string(),
            position: 100.0,
            avg_cost: 150.25,
            market_value: 15025.0,
        });
        let pos = tracker.get(265598).unwrap();
        assert_eq!(pos.position, 100.0);
        assert_eq!(pos.symbol, "AAPL");
    }

    #[test]
    fn position_tracker_update_replaces() {
        let mut tracker = PositionTracker::default();
        tracker.update(PositionUpdate {
            account_id: "DU12345".to_string(),
            con_id: 265598,
            symbol: "AAPL".to_string(),
            position: 100.0,
            avg_cost: 150.0,
            market_value: 15000.0,
        });
        tracker.update(PositionUpdate {
            account_id: "DU12345".to_string(),
            con_id: 265598,
            symbol: "AAPL".to_string(),
            position: 50.0,
            avg_cost: 155.0,
            market_value: 7750.0,
        });
        assert_eq!(tracker.get(265598).unwrap().position, 50.0);
    }

    // --- parse_account_value: all supported tags ---

    #[test]
    fn parse_account_code() {
        let mut s = AccountSummary::default();
        parse_account_value("AccountCode", "U999888", &mut s);
        assert_eq!(s.account_id, "U999888");
    }

    #[test]
    fn parse_net_liquidation() {
        let mut s = AccountSummary::default();
        parse_account_value("NetLiquidation", "55555.55", &mut s);
        assert_eq!(s.net_liquidation, 55555.55);
    }

    #[test]
    fn parse_total_cash_value() {
        let mut s = AccountSummary::default();
        parse_account_value("TotalCashValue", "12345.67", &mut s);
        assert_eq!(s.total_cash, 12345.67);
    }

    #[test]
    fn parse_buying_power() {
        let mut s = AccountSummary::default();
        parse_account_value("BuyingPower", "400000.0", &mut s);
        assert_eq!(s.buying_power, 400000.0);
    }

    #[test]
    fn parse_gross_position_value() {
        let mut s = AccountSummary::default();
        parse_account_value("GrossPositionValue", "75000.0", &mut s);
        assert_eq!(s.gross_position_value, 75000.0);
    }

    #[test]
    fn parse_maint_margin_req() {
        let mut s = AccountSummary::default();
        parse_account_value("MaintMarginReq", "25000.0", &mut s);
        assert_eq!(s.maintenance_margin, 25000.0);
    }

    #[test]
    fn parse_available_funds() {
        let mut s = AccountSummary::default();
        parse_account_value("AvailableFunds", "30000.0", &mut s);
        assert_eq!(s.available_funds, 30000.0);
    }

    #[test]
    fn parse_excess_liquidity() {
        let mut s = AccountSummary::default();
        parse_account_value("ExcessLiquidity", "45000.0", &mut s);
        assert_eq!(s.excess_liquidity, 45000.0);
    }

    #[test]
    fn parse_currency() {
        let mut s = AccountSummary::default();
        parse_account_value("Currency", "USD", &mut s);
        assert_eq!(s.currency, "USD");
    }

    #[test]
    fn parse_unknown_tag_is_noop() {
        let mut s = AccountSummary::default();
        let before = s.clone();
        parse_account_value("SomeUnknownTag", "irrelevant", &mut s);
        assert_eq!(s.account_id, before.account_id);
        assert_eq!(s.net_liquidation, before.net_liquidation);
        assert_eq!(s.total_cash, before.total_cash);
        assert_eq!(s.buying_power, before.buying_power);
        assert_eq!(s.gross_position_value, before.gross_position_value);
        assert_eq!(s.maintenance_margin, before.maintenance_margin);
        assert_eq!(s.available_funds, before.available_funds);
        assert_eq!(s.excess_liquidity, before.excess_liquidity);
        assert_eq!(s.currency, before.currency);
    }

    #[test]
    fn parse_invalid_numeric_falls_back_to_zero() {
        let mut s = AccountSummary::default();
        s.net_liquidation = 999.0; // set non-zero first
        parse_account_value("NetLiquidation", "not_a_number", &mut s);
        assert_eq!(s.net_liquidation, 0.0);
    }

    #[test]
    fn parse_empty_value_string_numeric_falls_back_to_zero() {
        let mut s = AccountSummary::default();
        parse_account_value("BuyingPower", "", &mut s);
        assert_eq!(s.buying_power, 0.0);
    }

    #[test]
    fn parse_empty_value_string_for_string_field() {
        let mut s = AccountSummary::default();
        parse_account_value("AccountCode", "", &mut s);
        assert_eq!(s.account_id, "");
    }

    // --- AccountSummary ---

    #[test]
    fn account_summary_default_values() {
        let s = AccountSummary::default();
        assert_eq!(s.account_id, "");
        assert_eq!(s.net_liquidation, 0.0);
        assert_eq!(s.total_cash, 0.0);
        assert_eq!(s.buying_power, 0.0);
        assert_eq!(s.gross_position_value, 0.0);
        assert_eq!(s.maintenance_margin, 0.0);
        assert_eq!(s.available_funds, 0.0);
        assert_eq!(s.excess_liquidity, 0.0);
        assert_eq!(s.currency, "");
    }

    #[test]
    fn account_summary_clone() {
        let mut s = AccountSummary::default();
        s.account_id = "DU111".to_string();
        s.net_liquidation = 50000.0;
        let cloned = s.clone();
        assert_eq!(cloned.account_id, "DU111");
        assert_eq!(cloned.net_liquidation, 50000.0);
    }

    #[test]
    fn account_summary_debug() {
        let s = AccountSummary::default();
        let debug_str = format!("{:?}", s);
        assert!(debug_str.contains("AccountSummary"));
    }

    // --- PositionUpdate ---

    #[test]
    fn position_update_debug() {
        let p = PositionUpdate {
            account_id: "DU1".to_string(),
            con_id: 1,
            symbol: "X".to_string(),
            position: 10.0,
            avg_cost: 5.0,
            market_value: 50.0,
        };
        let debug_str = format!("{:?}", p);
        assert!(debug_str.contains("PositionUpdate"));
    }

    #[test]
    fn position_update_clone() {
        let p = PositionUpdate {
            account_id: "DU2".to_string(),
            con_id: 42,
            symbol: "MSFT".to_string(),
            position: 200.0,
            avg_cost: 300.0,
            market_value: 60000.0,
        };
        let cloned = p.clone();
        assert_eq!(cloned.account_id, "DU2");
        assert_eq!(cloned.con_id, 42);
        assert_eq!(cloned.symbol, "MSFT");
        assert_eq!(cloned.position, 200.0);
        assert_eq!(cloned.avg_cost, 300.0);
        assert_eq!(cloned.market_value, 60000.0);
    }

    #[test]
    fn position_update_fields_accessible() {
        let p = PositionUpdate {
            account_id: "DU3".to_string(),
            con_id: 99,
            symbol: "TSLA".to_string(),
            position: -50.0,
            avg_cost: 250.0,
            market_value: -12500.0,
        };
        assert_eq!(p.account_id, "DU3");
        assert_eq!(p.con_id, 99);
        assert_eq!(p.symbol, "TSLA");
        assert_eq!(p.position, -50.0);
        assert_eq!(p.avg_cost, 250.0);
        assert_eq!(p.market_value, -12500.0);
    }

    // --- PositionTracker ---

    #[test]
    fn position_tracker_get_unknown_returns_none() {
        let tracker = PositionTracker::default();
        assert!(tracker.get(999).is_none());
    }

    #[test]
    fn position_tracker_update_then_get() {
        let mut tracker = PositionTracker::default();
        tracker.update(PositionUpdate {
            account_id: "DU1".to_string(),
            con_id: 100,
            symbol: "GOOG".to_string(),
            position: 10.0,
            avg_cost: 100.0,
            market_value: 1000.0,
        });
        let pos = tracker.get(100);
        assert!(pos.is_some());
        assert_eq!(pos.unwrap().symbol, "GOOG");
    }

    #[test]
    fn position_tracker_multiple_different_con_ids() {
        let mut tracker = PositionTracker::default();
        tracker.update(PositionUpdate {
            account_id: "DU1".to_string(),
            con_id: 1,
            symbol: "AAPL".to_string(),
            position: 10.0,
            avg_cost: 150.0,
            market_value: 1500.0,
        });
        tracker.update(PositionUpdate {
            account_id: "DU1".to_string(),
            con_id: 2,
            symbol: "MSFT".to_string(),
            position: 20.0,
            avg_cost: 300.0,
            market_value: 6000.0,
        });
        tracker.update(PositionUpdate {
            account_id: "DU1".to_string(),
            con_id: 3,
            symbol: "GOOG".to_string(),
            position: 5.0,
            avg_cost: 2800.0,
            market_value: 14000.0,
        });
        assert_eq!(tracker.get(1).unwrap().symbol, "AAPL");
        assert_eq!(tracker.get(2).unwrap().symbol, "MSFT");
        assert_eq!(tracker.get(3).unwrap().symbol, "GOOG");
        assert!(tracker.get(4).is_none());
    }

    #[test]
    fn position_tracker_all_returns_all_positions() {
        let mut tracker = PositionTracker::default();
        tracker.update(PositionUpdate {
            account_id: "DU1".to_string(),
            con_id: 10,
            symbol: "A".to_string(),
            position: 1.0,
            avg_cost: 1.0,
            market_value: 1.0,
        });
        tracker.update(PositionUpdate {
            account_id: "DU1".to_string(),
            con_id: 20,
            symbol: "B".to_string(),
            position: 2.0,
            avg_cost: 2.0,
            market_value: 4.0,
        });
        let all: Vec<&PositionUpdate> = tracker.all().collect();
        assert_eq!(all.len(), 2);
        let symbols: Vec<&str> = all.iter().map(|p| p.symbol.as_str()).collect();
        assert!(symbols.contains(&"A"));
        assert!(symbols.contains(&"B"));
    }

    #[test]
    fn position_tracker_all_empty() {
        let tracker = PositionTracker::default();
        let all: Vec<&PositionUpdate> = tracker.all().collect();
        assert!(all.is_empty());
    }

    // --- AccountCommand ---

    #[test]
    fn account_command_summary_update_variant() {
        let mut summary = AccountSummary::default();
        summary.account_id = "DU999".to_string();
        summary.net_liquidation = 77777.0;
        let cmd = AccountCommand::SummaryUpdate(summary);
        if let AccountCommand::SummaryUpdate(s) = cmd {
            assert_eq!(s.account_id, "DU999");
            assert_eq!(s.net_liquidation, 77777.0);
        } else {
            panic!("expected SummaryUpdate variant");
        }
    }

    #[test]
    fn account_command_position_update_variant() {
        let pos = PositionUpdate {
            account_id: "DU888".to_string(),
            con_id: 555,
            symbol: "NVDA".to_string(),
            position: 25.0,
            avg_cost: 800.0,
            market_value: 20000.0,
        };
        let cmd = AccountCommand::PositionUpdate(pos);
        if let AccountCommand::PositionUpdate(p) = cmd {
            assert_eq!(p.account_id, "DU888");
            assert_eq!(p.con_id, 555);
            assert_eq!(p.symbol, "NVDA");
        } else {
            panic!("expected PositionUpdate variant");
        }
    }
}
