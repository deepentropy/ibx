// Scenarios recorded from the reference gateway, replayed with the C++
// client as the API client: the scenario runner of the Rust tests plays
// the recorded servers on in-memory links, this driver makes each recorded
// request on an EClientSocket and hands back the callbacks of its wrapper
// in the form of the official client. They are compared with the ones the
// official client got from the reference: same callbacks, fields, count
// and order; the frames of the requests are compared on the way.
//
// No network. The scenarios are the ones of tests/python/test_scenario_replay.py.

#include <cstdio>
#include <map>
#include <sstream>

#include "DefaultEWrapper.h"
#include "EClientSocket.h"
#include "test_support.h"

using namespace ibxtest;

namespace {

// ── The callbacks as a JSON list, in the form of the official client ──

std::string quoted(const std::string& s)
{
  std::string out = "\"";
  for (unsigned char c : s) {
    if (c == '"' || c == '\\') {
      out += '\\';
      out += static_cast<char>(c);
    } else if (c < 0x20) {
      char code[8];
      std::snprintf(code, sizeof code, "\\u%04x", c);
      out += code;
    } else {
      out += static_cast<char>(c);
    }
  }
  return out + "\"";
}

std::string num(double v)
{
  if (v == UNSET_DOUBLE)
    return "\"MAX\"";
  char text[40];
  std::snprintf(text, sizeof text, "%.17g", v);
  return text;
}

std::string num(long long v) { return std::to_string(v); }
std::string num(int v) { return std::to_string(v); }
std::string flag(bool v) { return v ? "true" : "false"; }

// A quantity as its text, as the recorded callbacks have it.
std::string dec(Decimal v) { return v.isUnset() ? "\"MAX\"" : quoted(DecimalFunctions::decimalStringToDisplay(v)); }

std::string contractJson(const Contract& c)
{
  return "{\"conId\":" + num(c.conId) + ",\"symbol\":" + quoted(c.symbol) + ",\"secType\":" + quoted(c.secType) + ",\"exchange\":" + quoted(c.exchange)
       + ",\"primaryExchange\":" + quoted(c.primaryExchange) + "}";
}

std::string orderJson(const Order& o)
{
  return "{\"action\":" + quoted(o.action) + ",\"totalQuantity\":" + dec(o.totalQuantity) + ",\"orderType\":" + quoted(o.orderType) + ",\"lmtPrice\":"
       + num(o.lmtPrice) + ",\"auxPrice\":" + num(o.auxPrice) + ",\"tif\":" + quoted(o.tif) + ",\"ocaGroup\":" + quoted(o.ocaGroup) + ",\"orderRef\":"
       + quoted(o.orderRef) + ",\"parentId\":" + num(o.parentId) + ",\"outsideRth\":" + flag(o.outsideRth) + ",\"goodAfterTime\":" + quoted(o.goodAfterTime)
       + ",\"goodTillDate\":" + quoted(o.goodTillDate) + ",\"account\":" + quoted(o.account) + ",\"trailingPercent\":" + num(o.trailingPercent)
       + ",\"trailStopPrice\":" + num(o.trailStopPrice) + ",\"whatIf\":" + flag(o.whatIf) + ",\"permId\":" + num(o.permId) + ",\"clientId\":" + num(o.clientId)
       + ",\"ocaType\":" + num(o.ocaType) + ",\"clearingIntent\":" + quoted(o.clearingIntent) + ",\"shareholder\":" + quoted(o.shareholder)
       + ",\"deltaNeutralOrderType\":" + quoted(o.deltaNeutralOrderType) + ",\"adjustedOrderType\":" + quoted(o.adjustedOrderType) + ",\"submitter\":"
       + quoted(o.submitter) + "}";
}

std::string barJson(const Bar& b)
{
  return "{\"date\":" + quoted(b.time) + ",\"open\":" + num(b.open) + ",\"high\":" + num(b.high) + ",\"low\":" + num(b.low) + ",\"close\":" + num(b.close)
       + ",\"volume\":" + dec(b.volume) + ",\"wap\":" + dec(b.wap) + ",\"barCount\":" + num(b.count) + "}";
}

class Recorder : public DefaultEWrapper {
public:
  std::vector<std::string> calls;

  void add(const char* name, std::initializer_list<std::string> args)
  {
    std::string call = "[" + quoted(name);
    for (const std::string& a : args)
      call += "," + a;
    calls.push_back(call + "]");
  }

  void error(int id, long long, int errorCode, const std::string& errorString, const std::string& advancedOrderRejectJson) override
  {
    add("error", {num(id), "null", num(errorCode), quoted(errorString), quoted(advancedOrderRejectJson)});
  }
  void orderStatus(int orderId, const std::string& status, Decimal filled, Decimal remaining, double avgFillPrice, long long permId, int parentId,
                   double lastFillPrice, int clientId, const std::string& whyHeld, double mktCapPrice) override
  {
    add("orderStatus", {num(orderId), quoted(status), dec(filled), dec(remaining), num(avgFillPrice), num(permId), num(parentId), num(lastFillPrice),
                        num(clientId), quoted(whyHeld), num(mktCapPrice)});
  }
  void openOrder(int orderId, const Contract& contract, const Order& order, const OrderState& orderState) override
  {
    add("openOrder", {num(orderId), contractJson(contract), orderJson(order), "{\"status\":" + quoted(orderState.status) + "}"});
  }
  void openOrderEnd() override { add("openOrderEnd", {}); }
  void position(const std::string& account, const Contract& contract, Decimal position, double avgCost) override
  {
    add("position", {quoted(account), contractJson(contract), dec(position), num(avgCost)});
  }
  void positionEnd() override { add("positionEnd", {}); }
  void execDetails(int reqId, const Contract& contract, const Execution& e) override
  {
    add("execDetails", {num(reqId), contractJson(contract),
                        "{\"exchange\":" + quoted(e.exchange) + ",\"side\":" + quoted(e.side) + ",\"shares\":" + dec(e.shares) + ",\"price\":" + num(e.price)
                          + ",\"cumQty\":" + dec(e.cumQty) + ",\"avgPrice\":" + num(e.avgPrice) + ",\"orderId\":" + num(e.orderId) + ",\"orderRef\":"
                          + quoted(e.orderRef) + ",\"lastLiquidity\":" + num(e.lastLiquidity) + "}"});
  }
  void execDetailsEnd(int reqId) override { add("execDetailsEnd", {num(reqId)}); }
  void commissionAndFeesReport(const CommissionAndFeesReport& r) override
  {
    add("commissionAndFeesReport", {"{\"commissionAndFees\":" + num(r.commissionAndFees) + ",\"currency\":" + quoted(r.currency) + "}"});
  }
  void tickPrice(int reqId, TickType field, double price, const TickAttrib& a) override
  {
    add("tickPrice", {num(reqId), num(static_cast<int>(field)), num(price),
                      "{\"canAutoExecute\":" + flag(a.canAutoExecute) + ",\"pastLimit\":" + flag(a.pastLimit) + ",\"preOpen\":" + flag(a.preOpen) + "}"});
  }
  void tickSize(int reqId, TickType field, Decimal size) override { add("tickSize", {num(reqId), num(static_cast<int>(field)), dec(size)}); }
  void tickString(int reqId, TickType tickType, const std::string& value) override
  {
    add("tickString", {num(reqId), num(static_cast<int>(tickType)), quoted(value)});
  }
  void tickGeneric(int reqId, TickType tickType, double value) override { add("tickGeneric", {num(reqId), num(static_cast<int>(tickType)), num(value)}); }
  void tickSnapshotEnd(int reqId) override { add("tickSnapshotEnd", {num(reqId)}); }
  void marketDataType(int reqId, int type) override { add("marketDataType", {num(reqId), num(type)}); }
  void tickReqParams(int reqId, double minTick, const std::string& bboExchange, int snapshotPermissions) override
  {
    add("tickReqParams", {num(reqId), num(minTick), quoted(bboExchange), num(snapshotPermissions)});
  }
  void accountSummary(int reqId, const std::string& account, const std::string& tag, const std::string& value, const std::string& currency) override
  {
    add("accountSummary", {num(reqId), quoted(account), quoted(tag), quoted(value), quoted(currency)});
  }
  void accountSummaryEnd(int reqId) override { add("accountSummaryEnd", {num(reqId)}); }
  void historicalData(int reqId, const Bar& bar) override { add("historicalData", {num(reqId), barJson(bar)}); }
  void historicalDataUpdate(int reqId, const Bar& bar) override { add("historicalDataUpdate", {num(reqId), barJson(bar)}); }
  void historicalDataEnd(int reqId, const std::string& start, const std::string& end) override
  {
    add("historicalDataEnd", {num(reqId), quoted(start), quoted(end)});
  }
  void tickByTickAllLast(int reqId, int tickType, long long time, double price, Decimal size, const TickAttribLast& a, const std::string& exchange,
                         const std::string& specialConditions) override
  {
    add("tickByTickAllLast", {num(reqId), num(tickType), num(time), num(price), dec(size),
                              "{\"pastLimit\":" + flag(a.pastLimit) + ",\"unreported\":" + flag(a.unreported) + "}", quoted(exchange),
                              quoted(specialConditions)});
  }
  void tickByTickBidAsk(int reqId, long long time, double bidPrice, double askPrice, Decimal bidSize, Decimal askSize, const TickAttribBidAsk& a) override
  {
    add("tickByTickBidAsk", {num(reqId), num(time), num(bidPrice), num(askPrice), dec(bidSize), dec(askSize),
                             "{\"bidPastLow\":" + flag(a.bidPastLow) + ",\"askPastHigh\":" + flag(a.askPastHigh) + "}"});
  }
  void tickByTickMidPoint(int reqId, long long time, double midPoint) override { add("tickByTickMidPoint", {num(reqId), num(time), num(midPoint)}); }
  void historicalTicksLast(int reqId, const std::vector<HistoricalTickLast>& ticks, bool done) override
  {
    std::string rows = "[";
    for (size_t k = 0; k < ticks.size(); ++k) {
      const HistoricalTickLast& t = ticks[k];
      rows += std::string(k ? "," : "") + "{\"time\":" + num(t.time) + ",\"price\":" + num(t.price) + ",\"size\":" + dec(t.size) + ",\"exchange\":"
            + quoted(t.exchange) + ",\"specialConditions\":" + quoted(t.specialConditions) + ",\"tickAttribLast\":{\"pastLimit\":"
            + flag(t.tickAttribLast.pastLimit) + ",\"unreported\":" + flag(t.tickAttribLast.unreported) + "}}";
    }
    add("historicalTicksLast", {num(reqId), rows + "]", flag(done)});
  }
  void realtimeBar(int reqId, long long time, double open, double high, double low, double close, Decimal volume, Decimal wap, int count) override
  {
    add("realtimeBar", {num(reqId), num(time), num(open), num(high), num(low), num(close), dec(volume), dec(wap), num(count)});
  }
  void headTimestamp(int reqId, const std::string& headTimestamp) override { add("headTimestamp", {num(reqId), quoted(headTimestamp)}); }
  void scannerData(int reqId, int rank, const ContractDetails& details, const std::string& distance, const std::string& benchmark,
                   const std::string& projection, const std::string& legsStr) override
  {
    add("scannerData", {num(reqId), num(rank), "{\"contract\":" + contractJson(details.contract) + "}", quoted(distance), quoted(benchmark),
                        quoted(projection), quoted(legsStr)});
  }
  void scannerDataEnd(int reqId) override { add("scannerDataEnd", {num(reqId)}); }
  void completedOrder(const Contract& contract, const Order& o, const OrderState& orderState) override
  {
    add("completedOrder", {contractJson(contract),
                           "{\"action\":" + quoted(o.action) + ",\"totalQuantity\":" + dec(o.totalQuantity) + ",\"orderType\":" + quoted(o.orderType)
                             + ",\"lmtPrice\":" + num(o.lmtPrice) + ",\"auxPrice\":" + num(o.auxPrice) + ",\"tif\":" + quoted(o.tif) + ",\"orderRef\":"
                             + quoted(o.orderRef) + ",\"permId\":" + num(o.permId) + ",\"filledQuantity\":" + dec(o.filledQuantity) + "}",
                           "{\"status\":" + quoted(orderState.status) + "}"});
  }
  void completedOrdersEnd() override { add("completedOrdersEnd", {}); }
  void updateAccountValue(const std::string& key, const std::string& val, const std::string& currency, const std::string& accountName) override
  {
    add("updateAccountValue", {quoted(key), quoted(val), quoted(currency), quoted(accountName)});
  }
  void updatePortfolio(const Contract& contract, Decimal position, double marketPrice, double marketValue, double averageCost, double unrealizedPNL,
                       double realizedPNL, const std::string& accountName) override
  {
    add("updatePortfolio", {contractJson(contract), dec(position), num(marketPrice), num(marketValue), num(averageCost), num(unrealizedPNL),
                            num(realizedPNL), quoted(accountName)});
  }
  void updateAccountTime(const std::string& timeStamp) override { add("updateAccountTime", {quoted(timeStamp)}); }
  void accountDownloadEnd(const std::string& accountName) override { add("accountDownloadEnd", {quoted(accountName)}); }
  void pnl(int reqId, double dailyPnL, double unrealizedPnL, double realizedPnL) override
  {
    add("pnl", {num(reqId), num(dailyPnL), num(unrealizedPnL), num(realizedPnL)});
  }
  void pnlSingle(int reqId, Decimal pos, double dailyPnL, double unrealizedPnL, double realizedPnL, double value) override
  {
    add("pnlSingle", {num(reqId), dec(pos), num(dailyPnL), num(unrealizedPnL), num(realizedPnL), num(value)});
  }
  void smartComponents(int reqId, const SmartComponentsMap& theMap) override
  {
    std::string rows = "{";
    bool first = true;
    for (const auto& item : theMap) {
      rows += std::string(first ? "" : ",") + quoted(std::to_string(item.first)) + ":[" + quoted(std::get<0>(item.second)) + ","
            + quoted(std::string(1, std::get<1>(item.second))) + "]";
      first = false;
    }
    add("smartComponents", {num(reqId), rows + "}"});
  }
};

// ── The recorded requests ──

// The fields of a request: `contract.symbol`, `order.algoParams.0.tag`.
typedef std::map<std::string, std::string> Fields;

Fields parse(ibx_str text)
{
  Fields out;
  std::string all(text.p ? text.p : "", text.n);
  size_t at = 0;
  while (at < all.size()) {
    size_t end = all.find('\x1e', at);
    size_t mid = all.find('\x1f', at);
    if (end == std::string::npos || mid == std::string::npos || mid > end)
      break;
    out[all.substr(at, mid - at)] = all.substr(mid + 1, end - mid - 1);
    at = end + 1;
  }
  return out;
}

struct Request {
  const Fields& q;

  std::string text(const std::string& key, const std::string& otherwise = "") const
  {
    Fields::const_iterator it = q.find(key);
    return it == q.end() ? otherwise : it->second;
  }
  int integer(const std::string& key, int otherwise = 0) const
  {
    Fields::const_iterator it = q.find(key);
    return it == q.end() ? otherwise : static_cast<int>(std::stod(it->second));
  }
  bool flag(const std::string& key) const
  {
    std::string v = text(key, "false");
    return v == "true" || v == "1";
  }
  // The names directly under a prefix: `order.` gives action, lmtPrice, algoParams...
  std::set<std::string> under(const std::string& prefix) const
  {
    std::set<std::string> out;
    for (Fields::const_iterator it = q.lower_bound(prefix); it != q.end() && it->first.compare(0, prefix.size(), prefix) == 0; ++it) {
      std::string rest = it->first.substr(prefix.size());
      out.insert(rest.substr(0, rest.find('.')));
    }
    return out;
  }
};

class Driver {
public:
  Recorder wrapper;
  EClientSocket client;
  std::vector<std::string> problems;

  Driver() : client(&wrapper) {}

  Contract contract(const Request& r, const std::string& prefix)
  {
    Contract c;
    for (const std::string& name : r.under(prefix)) {
      std::string value = r.text(prefix + name);
      if (name == "comboLegs") {
        c.comboLegs = std::make_shared<Contract::ComboLegList>();
        for (int k = 0; k < r.integer(prefix + "comboLegs.#"); ++k) {
          std::string at = prefix + "comboLegs." + std::to_string(k) + ".";
          ComboLegSPtr leg = std::make_shared<ComboLeg>();
          leg->conId = r.integer(at + "conId");
          leg->ratio = r.integer(at + "ratio");
          leg->action = r.text(at + "action");
          leg->exchange = r.text(at + "exchange");
          c.comboLegs->push_back(leg);
        }
      } else if (name == "primaryExch") {
        c.primaryExchange = value;
      } else if (name == "strike") {
        c.strike = number(value);
      } else if (name == "multiplier") {
        // A number at session level 225, a text before.
        std::ostringstream text;
        text << std::stod(value);
        c.multiplier = value.find_first_not_of("0123456789.-") == std::string::npos ? text.str() : value;
      } else if (!setField(c, name, value)) {
        problems.push_back("contract field " + name);
      }
    }
    return c;
  }

  TagValueListSPtr tagList(const Request& r, const std::string& prefix)
  {
    TagValueListSPtr list = std::make_shared<TagValueList>();
    for (int k = 0; k < r.integer(prefix + "#"); ++k) {
      std::string at = prefix + std::to_string(k) + ".";
      list->push_back(std::make_shared<TagValue>(r.text(at + "tag"), r.text(at + "value")));
    }
    return list;
  }

  Order order(const Request& r, const std::string& prefix)
  {
    Order o;
    for (const std::string& name : r.under(prefix)) {
      std::string value = r.text(prefix + name);
      if (name == "softDollarTier" || name == "orderId") {
        continue;
      } else if (name == "algoParams") {
        o.algoParams = tagList(r, prefix + "algoParams.");
      } else if (name == "smartComboRoutingParams") {
        o.smartComboRoutingParams = tagList(r, prefix + "smartComboRoutingParams.");
      } else if (name == "orderComboLegs") {
        o.orderComboLegs = std::make_shared<Order::OrderComboLegList>();
        for (int k = 0; k < r.integer(prefix + "orderComboLegs.#"); ++k) {
          OrderComboLegSPtr leg = std::make_shared<OrderComboLeg>();
          leg->price = number(r.text(prefix + "orderComboLegs." + std::to_string(k) + ".price", "MAX"));
          o.orderComboLegs->push_back(leg);
        }
      } else if (name == "totalQuantity") {
        o.totalQuantity = value == "MAX" ? UNSET_DECIMAL : DecimalFunctions::stringToDecimal(value);
      } else if (name == "usePriceMgmtAlgo") {
        o.usePriceMgmtAlgo = value == "true" || value == "1" ? USE : DONT_USE;
      } else if (name == "routeMarketableToBbo") {
        o.routeMarketableToBbo = value == "true" || value == "1" ? STATE_YES : STATE_NO;
      } else if (name == "seekPriceImprovement") {
        o.seekPriceImprovement = value == "true" || value == "1" ? STATE_YES : STATE_NO;
      } else if (name == "origin") {
        o.origin = static_cast<Origin>(std::stoi(value));
      } else if (name == "referenceContractId") {
        o.referenceContractId = std::stoi(value);
      } else if (name == "refFuturesConId") {
        o.refFuturesConId = std::stoi(value);
      } else if (name == "adjustableTrailingUnit") {
        o.adjustableTrailingUnit = std::stoi(value);
      } else if (name == "parentPermId") {
        o.parentPermId = std::stoll(value);
      } else if (name == "peggedChangeAmount") {
        o.peggedChangeAmount = number(value);
      } else if (name == "referenceChangeAmount") {
        o.referenceChangeAmount = number(value);
      } else if (!setField(o, name, value)) {
        problems.push_back("order field " + name);
      }
    }
    return o;
  }

  bool request(const std::string& name, const Fields& fields)
  {
    Request q{fields};
    int rid = q.integer("reqId");
    EClientSocket& c = client;
    TagValueListSPtr none;
    if (name == "PLACE_ORDER") {
      c.placeOrder(q.integer("orderId"), contract(q, "contract."), order(q, "order."));
    } else if (name == "CANCEL_ORDER") {
      c.cancelOrder(q.integer("orderId"), OrderCancel());
    } else if (name == "REQ_MKT_DATA") {
      c.reqMktData(rid, contract(q, "contract."), q.text("genericTickList"), q.flag("snapshot"), q.flag("regulatorySnapshot"), none);
    } else if (name == "CANCEL_MKT_DATA") {
      c.cancelMktData(rid);
    } else if (name == "REQ_MARKET_DATA_TYPE") {
      c.reqMarketDataType(q.integer("marketDataType", 1));
    } else if (name == "REQ_ACCOUNT_SUMMARY") {
      c.reqAccountSummary(rid, q.text("group"), q.text("tags"));
    } else if (name == "CANCEL_ACCOUNT_SUMMARY") {
      c.cancelAccountSummary(rid);
    } else if (name == "REQ_HISTORICAL_DATA") {
      c.reqHistoricalData(rid, contract(q, "contract."), q.text("endDateTime"), q.text("duration"), q.text("barSizeSetting"), q.text("whatToShow"),
                          q.flag("useRTH") ? 1 : 0, q.integer("formatDate", 1), q.flag("keepUpToDate"), none);
    } else if (name == "CANCEL_HISTORICAL_DATA") {
      c.cancelHistoricalData(rid);
    } else if (name == "REQ_POSITIONS") {
      c.reqPositions();
    } else if (name == "REQ_ALL_OPEN_ORDERS") {
      c.reqAllOpenOrders();
    } else if (name == "REQ_GLOBAL_CANCEL") {
      c.reqGlobalCancel(OrderCancel());
    } else if (name == "CANCEL_POSITIONS") {
      c.cancelPositions();
    } else if (name == "REQ_CONTRACT_DATA") {
      c.reqContractDetails(rid, contract(q, "contract."));
    } else if (name == "REQ_TICK_BY_TICK_DATA") {
      c.reqTickByTickData(rid, contract(q, "contract."), q.text("tickType"), q.integer("numberOfTicks"), q.flag("ignoreSize"));
    } else if (name == "REQ_HISTORICAL_TICKS") {
      c.reqHistoricalTicks(rid, contract(q, "contract."), q.text("startDateTime"), q.text("endDateTime"), q.integer("numberOfTicks"), q.text("whatToShow"),
                           q.flag("useRTH") ? 1 : 0, q.flag("ignoreSize"), none);
    } else if (name == "CANCEL_TICK_BY_TICK_DATA") {
      c.cancelTickByTickData(rid);
    } else if (name == "REQ_REAL_TIME_BARS") {
      c.reqRealTimeBars(rid, contract(q, "contract."), q.integer("barSize", 5), q.text("whatToShow"), q.flag("useRTH"), none);
    } else if (name == "CANCEL_REAL_TIME_BARS") {
      c.cancelRealTimeBars(rid);
    } else if (name == "REQ_OPEN_ORDERS") {
      c.reqOpenOrders();
    } else if (name == "REQ_COMPLETED_ORDERS") {
      c.reqCompletedOrders(q.flag("apiOnly"));
    } else if (name == "REQ_SMART_COMPONENTS") {
      c.reqSmartComponents(rid, q.text("bboExchange"));
    } else if (name == "REQ_ACCT_DATA") {
      c.reqAccountUpdates(q.flag("subscribe"), q.text("acctCode"));
    } else if (name == "REQ_PNL") {
      c.reqPnL(rid, q.text("account"), q.text("modelCode"));
    } else if (name == "CANCEL_PNL") {
      c.cancelPnL(rid);
    } else if (name == "REQ_PNL_SINGLE") {
      c.reqPnLSingle(rid, q.text("account"), q.text("modelCode"), q.integer("conId"));
    } else if (name == "CANCEL_PNL_SINGLE") {
      c.cancelPnLSingle(rid);
    } else if (name == "REQ_SCANNER_SUBSCRIPTION") {
      ScannerSubscription sub;
      sub.numberOfRows = q.integer("scannerSubscription.numberOfRows", -1);
      sub.instrument = q.text("scannerSubscription.instrument");
      sub.locationCode = q.text("scannerSubscription.locationCode");
      sub.scanCode = q.text("scannerSubscription.scanCode");
      c.reqScannerSubscription(rid, sub, none, none);
    } else if (name == "CANCEL_SCANNER_SUBSCRIPTION") {
      c.cancelScannerSubscription(rid);
    } else if (name == "REQ_HEAD_TIMESTAMP") {
      c.reqHeadTimestamp(rid, contract(q, "contract."), q.text("whatToShow"), q.flag("useRTH") ? 1 : 0, q.integer("formatDate", 1));
    } else if (name == "CANCEL_HEAD_TIMESTAMP") {
      c.cancelHeadTimestamp(rid);
    } else if (name == "REQ_MKT_DEPTH") {
      c.reqMktDepth(rid, contract(q, "contract."), q.integer("numRows"), q.flag("isSmartDepth"), none);
    } else if (name == "CANCEL_MKT_DEPTH") {
      c.cancelMktDepth(rid, q.flag("isSmartDepth"));
    } else if (name == "REQ_EXECUTIONS") {
      ExecutionFilter filter;
      filter.m_clientId = q.integer("executionFilter.clientId");
      filter.m_acctCode = q.text("executionFilter.acctCode");
      filter.m_time = q.text("executionFilter.time");
      filter.m_symbol = q.text("executionFilter.symbol");
      filter.m_secType = q.text("executionFilter.secType");
      filter.m_exchange = q.text("executionFilter.exchange");
      filter.m_side = q.text("executionFilter.side");
      c.reqExecutions(rid, filter);
    } else {
      return false;
    }
    return true;
  }
};

void adopt(void* user, ibx_client* session) { static_cast<Driver*>(user)->client.ibxAdopt(session); }

bool request(void* user, ibx_str name, ibx_str fields)
{
  Driver* d = static_cast<Driver*>(user);
  try {
    return d->request(ibx::str(name), parse(fields));
  } catch (const std::exception& e) {
    d->problems.push_back(ibx::str(name) + ": " + e.what());
    return false;
  }
}

void dispatch(void* user, ibx_test_sink sink, void* sinkUser)
{
  Driver* d = static_cast<Driver*>(user);
  d->client.ibxProcess();
  std::string calls = "[";
  for (size_t k = 0; k < d->wrapper.calls.size(); ++k)
    calls += (k ? "," : "") + d->wrapper.calls[k];
  calls += "]";
  d->wrapper.calls.clear();
  sink(sinkUser, ibx::view(calls));
}

// A scenario and its options, as the Python replay tests run it.
struct Scenario {
  const char* name;
  const char* options;
};

const Scenario scenarios[] = {
#include "replays.inc"
};

}  // namespace

int runReplays()
{
  int failures = 0;
  int count = 0;
  for (const Scenario& s : scenarios) {
    Driver driver;
    ibx_test_driver hooks = {&driver, &adopt, &request, &dispatch};
    std::string report;
    bool ran = ibx_test_replay(ibx::view(s.name), ibx::view(s.options), &hooks, &takeText, &report);
    for (const std::string& p : driver.problems)
      report += "driver: " + p + "\n";
    ++count;
    if (!ran || !report.empty()) {
      ++failures;
      std::printf("FAILED replay %s [%s]\n%s\n", s.name, s.options, report.c_str());
    }
  }
  std::printf("%d replays, %d failed\n", count, failures);
  return failures;
}
