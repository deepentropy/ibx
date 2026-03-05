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
}
