// The offline tests of the protobuf forms (IBX_PROTOBUF): each protobuf
// callback comes before the plain callback of its answer with its fields
// set, a callback left to DefaultEWrapper costs one message and no more,
// and each protobuf request makes the plain request of its message.
//
// Needs the Rust client built with its test helpers (IBX_BUILD_TESTS).

#include <functional>
#include <map>
#include <memory>
#include <stdexcept>

#include "DefaultEWrapper.h"
#include "EClientSocket.h"
#include "EReaderOSSignal.h"
#include "PriceCondition.h"
#include "TimeCondition.h"
#include "ibx_proto.h"
#include "test_support.h"

namespace ibxtest {
#include "generated_proto_twins.inc"
}  // namespace ibxtest

using namespace ibxtest;

namespace {

int failed = 0;
int passed = 0;

void report(const char* test, const Failures& fails)
{
  if (fails.empty()) {
    ++passed;
    return;
  }
  ++failed;
  std::printf("FAILED %s\n", test);
  for (const std::string& f : fails)
    std::printf("  %s\n", f.c_str());
}

bool contains(const std::string& text, const std::string& part) { return text.find(part) != std::string::npos; }

template <class T> std::string describe(const char* kind, const T& x)
{
  ibx::Buf b;
  ibx::write(b.w, x);
  std::string text;
  if (!ibx_test_describe(ibx::view(kind), b.view(), &takeText, &text))
    text = "unknown kind";
  return text;
}

std::string made(const char* kind, int seed)
{
  std::string data;
  ibx_test_make(ibx::view(kind), seed, &takeText, &data);
  return data;
}

// The fields of two descriptions that differ, for a failure's report.
void differences(const std::string& what, const std::string& a, const std::string& b, Failures& fails)
{
  if (a == b)
    return;
  fails.push_back(what);
  size_t ia = 0, ib = 0;
  int shown = 0;
  while (ia < a.size() && ib < b.size() && shown < 12) {
    size_t ea = a.find(", ", ia), eb = b.find(", ", ib);
    std::string ta = a.substr(ia, ea == std::string::npos ? std::string::npos : ea - ia);
    std::string tb = b.substr(ib, eb == std::string::npos ? std::string::npos : eb - ib);
    if (ta != tb) {
      fails.push_back("    from the message: " + ta + "   the object: " + tb);
      ++shown;
    }
    if (ea == std::string::npos || eb == std::string::npos)
      break;
    ia = ea + 2;
    ib = eb + 2;
  }
}

std::string quantity(double v) { return ibx::pb::decimalText(ibx::decimalFromDouble(v)); }

// A wrapper derived from EWrapper: it gets every message, and keeps them.
class Messages : public CheckWrapper {
public:
  std::map<std::string, std::vector<std::shared_ptr<google::protobuf::Message>>> messages;

  void proto(const char* name, const google::protobuf::Message& m) override
  {
    sequence.push_back(name);
    std::shared_ptr<google::protobuf::Message> copy(m.New());
    copy->CopyFrom(m);
    messages[name].push_back(copy);
  }

  template <class M> const M* first(const char* name)
  {
    return messages[name].empty() ? nullptr : dynamic_cast<const M*>(messages[name][0].get());
  }

  // Where a callback came first; -1 when it did not.
  int at(const std::string& name) const
  {
    for (size_t k = 0; k < sequence.size(); ++k) {
      if (sequence[k] == name)
        return static_cast<int>(k);
    }
    return -1;
  }
};

std::string joined(const std::vector<std::string>& sequence)
{
  std::string out;
  for (const std::string& name : sequence)
    out += (out.empty() ? "" : " ") + name;
  return out;
}

// ── Each protobuf callback before its plain callback, with its fields ──

void everyCallback()
{
  Messages w;
  ibx::Sink sink{&w, nullptr};
  ibx_test_fire(&ibx::callbackTable, &sink);
  sink.flush();
  Failures fails = w.fails;
  expect(fails, "an exception was kept", !sink.error);
  for (const char* name : expectedCallbacks)
    expect(fails, std::string("plain callback once: ") + name, w.hits.count(name) == 1);

  // The callbacks whose message is made from their arguments or objects.
  for (const auto& pair : protoPairs) {
    int plain = w.at(pair[0]);
    expect(fails, std::string(pair[1]) + " right before " + pair[0], plain > 0 && w.sequence[plain - 1] == pair[1]);
  }
  // The ones with a rule of their own. The price waits for the callback
  // after it, which is not its size here: each has its message.
  const char* const own[][2] = {{"tickPrice", "tickPriceProtoBuf"},   {"tickSize", "tickSizeProtoBuf"},       {"tickReqParams", "tickReqParamsProtoBuf"},
                                {"error", "errorProtoBuf"},           {"historicalData", "historicalDataProtoBuf"}, {"scannerData", "scannerDataProtoBuf"}};
  for (const auto& pair : own) {
    int plain = w.at(pair[0]);
    expect(fails, std::string(pair[1]) + " right before " + pair[0], plain > 0 && w.sequence[plain - 1] == pair[1]);
  }
  expect(fails, "the three tick-by-tick callbacks have their message", w.messages["tickByTickDataProtoBuf"].size() == 3);
  expect(fails, "one message for the scan, none for its end",
         w.messages["scannerDataProtoBuf"].size() == 1 && w.at("scannerDataEnd") == w.at("scannerData") + 1);
  // The protobuf callbacks the client never gives: their plain callback is not given either.
  for (const char* never : {"orderBoundProtoBuf", "rerouteMarketDataRequestProtoBuf", "rerouteMarketDepthRequestProtoBuf", "configResponseProtoBuf",
                            "updateConfigResponseProtoBuf"})
    expect(fails, std::string("not given: ") + never, w.messages.count(never) == 0);
  expect(fails, "every other protobuf callback was given",
         w.messages.size() == static_cast<size_t>(ibx::PROTO_CALLBACK_COUNT) - 5);

  // Order status: the arguments, a quantity as its text.
  if (const protobuf::OrderStatus* m = w.first<protobuf::OrderStatus>("orderStatusProtoBuf")) {
    expect(fails, "orderStatus ids", m->orderid() == tvI("order_id", 0) && m->permid() == tvI("perm_id", 0) && m->parentid() == tvI("parent_id", 0)
                                        && m->clientid() == tvI("client_id", 0));
    expect(fails, "orderStatus texts", m->status() == tvS("status", 0) && m->whyheld() == tvS("why_held", 0));
    expect(fails, "orderStatus quantities", m->filled() == quantity(tvF("filled", 0)) && m->remaining() == quantity(tvF("remaining", 0)));
    expect(fails, "orderStatus prices", m->avgfillprice() == tvF("avg_fill_price", 0) && m->lastfillprice() == tvF("last_fill_price", 0)
                                           && m->mktcapprice() == tvF("mkt_cap_price", 0));
  } else {
    fails.push_back("no orderStatus message");
  }

  // Open order: the id, the contract, the order and its state.
  if (const protobuf::OpenOrder* m = w.first<protobuf::OpenOrder>("openOrderProtoBuf")) {
    expect(fails, "openOrder id", m->orderid() == tvI("order_id", 0));
    expect(fails, "openOrder contract", m->has_contract() && m->contract().conid() == tvI("conId", 1) && m->contract().symbol() == tvS("symbol", 1)
                                          && m->contract().primaryexch() == tvS("primaryExchange", 1) && m->contract().combolegs_size() == 2
                                          && m->contract().combolegs(1).action() == tvS("action", 3));
    expect(fails, "openOrder order", m->has_order() && m->order().action() == tvS("action", 1) && m->order().lmtprice() == tvF("lmtPrice", 1)
                                       && m->order().orderid() == tvI("orderId", 1) && m->order().has_origin() && m->order().has_exemptcode()
                                       && m->order().account() == tvS("account", 1));
    expect(fails, "openOrder order: the options of its algo", m->order().algostrategy() == tvS("algoStrategy", 1) && m->order().algoparams_size() == 2);
    expect(fails, "openOrder state", m->has_orderstate() && m->orderstate().status() == tvS("status", 1)
                                       && m->orderstate().commissionandfees() == tvF("commissionAndFees", 1) && m->orderstate().orderallocations_size() == 2);
  } else {
    fails.push_back("no openOrder message");
  }

  // Error: one of the servers has its message.
  if (const protobuf::ErrorMessage* m = w.first<protobuf::ErrorMessage>("errorProtoBuf")) {
    expect(fails, "error", m->id() == tvI("req_id", 0) && m->errorcode() == tvI("error_code", 0) && m->errormsg() == tvS("error_string", 0)
                             && m->advancedorderrejectjson() == tvS("advanced_order_reject_json", 0) && m->errortime() > 0);
  } else {
    fails.push_back("no error message");
  }

  // Contract data: the contract with its last trade date, time and zone
  // put together, and the details.
  if (const protobuf::ContractData* m = w.first<protobuf::ContractData>("contractDataProtoBuf")) {
    expect(fails, "contractData contract", m->reqid() == tvI("req_id", 0) && m->contract().symbol() == tvS("symbol", 2)
                                             && m->contract().lasttradedateorcontractmonth()
                                                  == tvS("lastTradeDateOrContractMonth", 2) + " " + tvS("lastTradeTime", 1) + " " + tvS("timeZoneId", 1));
    expect(fails, "contractData details", m->contractdetails().longname() == tvS("longName", 1)
                                            && m->contractdetails().mintick() == ibx::pb::floatText(tvF("minTick", 1))
                                            && m->contractdetails().secidlist_size() == 2 && m->contractdetails().ineligibilityreasonlist_size() == 2);
  } else {
    fails.push_back("no contractData message");
  }
  if (const protobuf::ContractData* m = w.first<protobuf::ContractData>("bondContractDataProtoBuf"))
    expect(fails, "bondContractData: the maturity is the date", m->contract().lasttradedateorcontractmonth().compare(0, tvS("maturity", 1).size(), tvS("maturity", 1)) == 0);
  else
    fails.push_back("no bondContractData message");

  // Historical data: the bars of the answer.
  if (const protobuf::HistoricalData* m = w.first<protobuf::HistoricalData>("historicalDataProtoBuf")) {
    expect(fails, "historicalData", m->reqid() == tvI("req_id", 0) && m->historicaldatabars_size() == 1 && m->historicaldatabars(0).date() == tvS("time", 1)
                                      && m->historicaldatabars(0).high() == tvF("high", 1) && m->historicaldatabars(0).barcount() == tvI("count", 1)
                                      && m->historicaldatabars(0).wap() == quantity(tvF("wap", 1)));
  } else {
    fails.push_back("no historicalData message");
  }

  // Scanner data: the rows of the scan.
  if (const protobuf::ScannerData* m = w.first<protobuf::ScannerData>("scannerDataProtoBuf")) {
    expect(fails, "scannerData", m->reqid() == tvI("req_id", 0) && m->scannerdataelement_size() == 1 && m->scannerdataelement(0).rank() == tvI("rank", 0)
                                   && m->scannerdataelement(0).distance() == tvS("distance", 0) && m->scannerdataelement(0).combokey() == tvS("legs_str", 0)
                                   && m->scannerdataelement(0).marketname() == tvS("marketName", 1)
                                   && m->scannerdataelement(0).contract().symbol() == tvS("symbol", 2));
  } else {
    fails.push_back("no scannerData message");
  }

  // A price without its size: no size, and the mask of its attributes.
  if (const protobuf::TickPrice* m = w.first<protobuf::TickPrice>("tickPriceProtoBuf"))
    expect(fails, "tickPrice", m->reqid() == tvI("req_id", 0) && m->price() == tvF("price", 0) && !m->has_size() && m->attrmask() == 5);
  else
    fails.push_back("no tickPrice message");
  if (!fails.empty())
    fails.push_back("callbacks: " + joined(w.sequence));
  report("each protobuf callback before its plain callback", fails);
}

// ── The messages that do not follow their plain callback one for one ──

void pricesBarsAndRows()
{
  const ibx_callbacks& t = ibx::callbackTable;
  Failures fails;
  {
    // A price, then its size: one message with the size, then the two
    // plain callbacks; the size has no message of its own.
    Messages w;
    ibx::Sink sink{&w, nullptr};
    t.tick_price(&sink, 7, BID, 150.25, 1);
    expect(fails, "the price waits for the next callback", w.sequence.empty());
    t.tick_size(&sink, 7, BID_SIZE, 200.0);
    expect(fails, "price with its size", joined(w.sequence) == "tickPriceProtoBuf tickPrice tickSize");
    const protobuf::TickPrice* m = w.first<protobuf::TickPrice>("tickPriceProtoBuf");
    expect(fails, "the message carries the size", m && m->size() == "200" && m->price() == 150.25 && m->ticktype() == BID && m->attrmask() == 1);
    // A size of another request, of another tick, and a price alone at the end of the pass.
    w.sequence.clear();
    t.tick_price(&sink, 7, ASK, 150.5, 0);
    t.tick_size(&sink, 8, ASK_SIZE, 300.0);
    t.tick_price(&sink, 7, LAST, 150.3, 0);
    t.tick_size(&sink, 7, VOLUME, 1000.0);
    t.tick_price(&sink, 7, DELAYED_LAST, 150.4, 0);
    t.tick_size(&sink, 7, DELAYED_LAST_SIZE, 10.0);
    t.tick_price(&sink, 7, HIGH, 151.0, 0);
    expect(fails, "the last price still waits", w.hits.count("tickPrice") == 4);
    sink.flush();
    expect(fails, "sizes that are not the price's",
           joined(w.sequence) == "tickPriceProtoBuf tickPrice tickSizeProtoBuf tickSize tickPriceProtoBuf tickPrice tickSizeProtoBuf tickSize "
                                 "tickPriceProtoBuf tickPrice tickSize tickPriceProtoBuf tickPrice");
    expect(fails, "no exception", !sink.error);
  }
  {
    // The bars of an answer: one message, then each bar, then the end.
    Messages w;
    ibx::Sink sink{&w, nullptr};
    std::string bar1 = made("Bar", 1), bar2 = made("Bar", 2), bar3 = made("Bar", 3);
    t.historical_data(&sink, 5, ibx::view(bar1));
    t.historical_data(&sink, 5, ibx::view(bar2));
    t.historical_data(&sink, 5, ibx::view(bar3));
    expect(fails, "the bars wait for the next callback", w.sequence.empty());
    t.historical_data_end(&sink, 5, ibx::view("start"), ibx::view("end"));
    // The bars of two requests one after the other: a message for each.
    t.historical_data(&sink, 6, ibx::view(bar1));
    t.historical_data(&sink, 9, ibx::view(bar2));
    sink.flush();
    expect(fails, "bars", joined(w.sequence) == "historicalDataProtoBuf historicalData historicalData historicalData historicalDataEndProtoBuf "
                                                "historicalDataEnd historicalDataProtoBuf historicalData historicalDataProtoBuf historicalData");
    const protobuf::HistoricalData* m = w.first<protobuf::HistoricalData>("historicalDataProtoBuf");
    expect(fails, "the message has the three bars", m && m->reqid() == 5 && m->historicaldatabars_size() == 3
                                                      && m->historicaldatabars(2).date() == tvS("time", 3) && m->historicaldatabars(1).close() == tvF("close", 2));
  }
  {
    // The rows of a scan: one message, each row, the end; a scan with no
    // row has its message all the same.
    Messages w;
    ibx::Sink sink{&w, nullptr};
    std::string row = made("ContractDetails", 1);
    t.scanner_data(&sink, 4, 0, ibx::view(row), ibx::view("d0"), ibx::view(""), ibx::view(""), ibx::view(""));
    t.scanner_data(&sink, 4, 1, ibx::view(row), ibx::view("d1"), ibx::view(""), ibx::view(""), ibx::view("legs"));
    t.scanner_data_end(&sink, 4);
    t.scanner_data_end(&sink, 3);
    expect(fails, "scan", joined(w.sequence) == "scannerDataProtoBuf scannerData scannerData scannerDataEnd scannerDataProtoBuf scannerDataEnd");
    const protobuf::ScannerData* m = w.first<protobuf::ScannerData>("scannerDataProtoBuf");
    expect(fails, "the message has the two rows", m && m->reqid() == 4 && m->scannerdataelement_size() == 2 && m->scannerdataelement(0).has_rank()
                                                    && m->scannerdataelement(0).rank() == 0 && m->scannerdataelement(1).distance() == "d1"
                                                    && m->scannerdataelement(1).combokey() == "legs" && !m->scannerdataelement(0).has_combokey());
    const protobuf::ScannerData* empty = w.messages["scannerDataProtoBuf"].size() == 2
                                           ? dynamic_cast<const protobuf::ScannerData*>(w.messages["scannerDataProtoBuf"][1].get()) : nullptr;
    expect(fails, "a scan with no row", empty && empty->reqid() == 3 && empty->scannerdataelement_size() == 0);
  }
  {
    // An error the official client makes itself has no message.
    Messages w;
    ibx::Sink sink{&w, nullptr};
    t.error(&sink, 3, 504, ibx::view("Not connected"), ibx::view(""));
    t.error(&sink, 3, 200, ibx::view("No security definition has been found for the request"), ibx::view(""));
    expect(fails, "errors", joined(w.sequence) == "error errorProtoBuf error");
    const protobuf::ErrorMessage* m = w.first<protobuf::ErrorMessage>("errorProtoBuf");
    expect(fails, "the message of the error", m && m->id() == 3 && m->errorcode() == 200 && !m->has_advancedorderrejectjson());
  }
  {
    // An exception of a protobuf callback is kept and its plain callback
    // is not given; the callbacks after it come.
    struct Throwing : Messages {
      void proto(const char* name, const google::protobuf::Message& m) override
      {
        Messages::proto(name, m);
        if (std::string(name) == "nextValidIdProtoBuf")
          throw std::runtime_error("from a protobuf callback");
      }
    } w;
    ibx::Sink sink{&w, nullptr};
    t.next_valid_id(&sink, 12);
    t.current_time(&sink, 34);
    expect(fails, "exception of a protobuf callback", bool(sink.error) && joined(w.sequence) == "nextValidIdProtoBuf currentTimeProtoBuf currentTime");
  }
  report("prices with their size, bars, scan rows, errors", fails);
}

// ── Values as the messages carry them ──

void values()
{
  Failures fails;
  using ibx::pb::floatText;
  expect(fails, "whole number", floatText(1.0) == "1.0" && floatText(100.0) == "100.0" && floatText(-2.0) == "-2.0" && floatText(0.0) == "0.0");
  expect(fails, "decimals", floatText(0.01) == "0.01" && floatText(0.0001) == "0.0001" && floatText(150.25) == "150.25" && floatText(0.1) == "0.1");
  expect(fails, "small and large", floatText(0.00001) == "1e-05" && floatText(1e16) == "1e+16" && floatText(1.2345678901234568e17) == "1.2345678901234568e+17"
                                     && floatText(1234567890123456.0) == "1234567890123456.0" && floatText(1.5e-7) == "1.5e-07");
  expect(fails, "quantity", ibx::pb::decimalText(DecimalFunctions::stringToDecimal("100")) == "100"
                              && ibx::pb::decimalText(DecimalFunctions::stringToDecimal("0.5")) == "0.5");
  double v = 0;
  expect(fails, "a text that is a number", ibx::pb::number("100", v) && v == 100 && ibx::pb::number(" 0.5 ", v) && v == 0.5 && !ibx::pb::number("", v)
                                             && !ibx::pb::number("1x", v));
  expect(fails, "a number kept as a text", ibx::pb::floatMaxText(100.0) == "100" && ibx::pb::floatMaxText(0.5) == "0.5" && ibx::pb::floatMaxText(UNSET_DOUBLE).empty()
                                             && ibx::pb::floatMaxText(0.123456789) == "0.12345679");
  expect(fails, "errors the official client makes itself", ibx::pb::clientError(504) && ibx::pb::clientError(502) && !ibx::pb::clientError(200)
                                                             && !ibx::pb::clientError(321) && !ibx::pb::clientError(2104));

  // A field with the value its object starts with is left out; the ones
  // the reference sets all the same are set.
  protobuf::Order order;
  ibx::pb::rows(Order(), order);
  expect(fails, "an order as it starts", order.has_orderid() && order.has_clientid() && order.has_origin() && order.has_exemptcode() && order.has_parentid()
                                           && order.has_triggermethod() && !order.has_transmit() && !order.has_lmtprice() && !order.has_action()
                                           && !order.has_totalquantity() && !order.has_usepricemgmtalgo() && !order.has_referencecontractid());
  Order changed;
  changed.lmtPrice = 0;
  changed.totalQuantity = DecimalFunctions::stringToDecimal("100");
  changed.usePriceMgmtAlgo = USE;
  changed.routeMarketableToBbo = STATE_NO;
  changed.outsideRth = true;
  changed.referenceContractId = 7;
  protobuf::Order shown;
  ibx::pb::rows(changed, shown);
  expect(fails, "an order with values", shown.has_lmtprice() && shown.lmtprice() == 0 && shown.totalquantity() == "100" && shown.usepricemgmtalgo() == 1
                                          && shown.has_routemarketabletobbo() && shown.routemarketabletobbo() == 0 && shown.outsiderth()
                                          && shown.referencecontractid() == 7);
  protobuf::Contract contract;
  Contract c;
  ibx::pb::rows(c, contract);
  expect(fails, "a contract as it starts", contract.ByteSizeLong() == 0);
  c.strike = 0;
  c.multiplier = "100";
  ibx::pb::rows(c, contract);
  expect(fails, "a strike of 0 and a multiplier", contract.has_strike() && contract.strike() == 0 && contract.multiplier() == 100);
  protobuf::Execution execution;
  ibx::pb::rows(Execution(), execution);
  expect(fails, "an execution as it starts", execution.has_clientid() && execution.optexerciseorlapsetype() == -1 && !execution.has_orderid());
  report("values as the messages carry them", fails);
}

// ── A callback left to DefaultEWrapper costs one message ──

void noCostWhenNotUsed()
{
  Failures fails;
  struct One : DefaultEWrapper {
    int statuses = 0;
    void orderStatusProtoBuf(const protobuf::OrderStatus&) override { ++statuses; }
  } w;
  unsigned long long before = ibx::protoMessagesBuilt;
  {
    ibx::Sink sink{&w, nullptr};
    ibx_test_fire(&ibx::callbackTable, &sink);
    sink.flush();
  }
  unsigned long long first = ibx::protoMessagesBuilt - before;
  // Every protobuf callback the client gives, once: the three tick-by-tick
  // callbacks share theirs.
  expect(fails, "the first pass builds each message once", first == static_cast<unsigned long long>(ibx::PROTO_CALLBACK_COUNT) - 5);
  for (int pass = 0; pass < 2; ++pass) {
    before = ibx::protoMessagesBuilt;
    ibx::Sink sink{&w, nullptr};
    ibx_test_fire(&ibx::callbackTable, &sink);
    sink.flush();
    expect(fails, "the next passes build the overridden one only", ibx::protoMessagesBuilt - before == 1);
  }
  expect(fails, "the overridden callback gets every message", w.statuses == 3);

  // A wrapper that overrides none: nothing is built after the first pass,
  // and a price no longer waits for its size.
  struct Plain : DefaultEWrapper {
    std::vector<std::string> sequence;
    void tickPrice(int, TickType, double, const TickAttrib&) override { sequence.push_back("tickPrice"); }
    void tickSize(int, TickType, Decimal) override { sequence.push_back("tickSize"); }
  } plain;
  {
    ibx::Sink sink{&plain, nullptr};
    ibx_test_fire(&ibx::callbackTable, &sink);
    sink.flush();
  }
  before = ibx::protoMessagesBuilt;
  plain.sequence.clear();
  ibx::Sink sink{&plain, nullptr};
  ibx_test_fire(&ibx::callbackTable, &sink);
  ibx::callbackTable.tick_price(&sink, 7, BID, 150.25, 0);
  expect(fails, "the price is given at once", plain.sequence.size() == 3 && plain.sequence.back() == "tickPrice");
  expect(fails, "nothing is built for a wrapper that overrides none", ibx::protoMessagesBuilt == before);

  // A wrapper derived from EWrapper gets every message, every time.
  Messages every;
  for (int pass = 0; pass < 2; ++pass) {
    before = ibx::protoMessagesBuilt;
    ibx::Sink s{&every, nullptr};
    ibx_test_fire(&ibx::callbackTable, &s);
    s.flush();
    expect(fails, "a wrapper derived from EWrapper gets every message", ibx::protoMessagesBuilt - before == static_cast<unsigned long long>(ibx::PROTO_CALLBACK_COUNT) - 3);
  }
  report("no message for a callback that is not used", fails);
}

// ── Each protobuf request makes the plain request of its message ──

struct Errors : DefaultEWrapper {
  std::vector<std::pair<int, int>> errors;
  std::vector<std::string> texts;
  void error(int id, long long, int errorCode, const std::string& errorString, const std::string&) override
  {
    errors.push_back({id, errorCode});
    texts.push_back(errorString);
  }
};

void requestsReachTheirPlainRequest()
{
  Errors w;
  EReaderOSSignal signal(10);
  EClientSocket client(&w, &signal);
  Failures fails;
  // A client that is not connected answers each plain request with the
  // error 504 for the request or order the message names.
  size_t count = 0;
#define X(request, message) \
  { \
    protobuf::message p; \
    p.set_reqid(77); \
    client.request(p); \
    ++count; \
    expect(fails, #request ": 504 for the request", w.errors.size() == count && w.errors.back() == std::make_pair(77, 504)); \
  }
  IBX_TEST_PROTO_REQUESTS_REQ_ID(X)
#undef X
#define X(request, message) \
  { \
    protobuf::message p; \
    p.set_orderid(78); \
    client.request(p); \
    ++count; \
    expect(fails, #request ": 504 for the order", w.errors.size() == count && w.errors.back() == std::make_pair(78, 504)); \
  }
  IBX_TEST_PROTO_REQUESTS_ORDER_ID(X)
#undef X
#define X(request, message) \
  { \
    client.request(protobuf::message()); \
    ++count; \
    expect(fails, #request ": 504", w.errors.size() == count && w.errors.back() == std::make_pair(-1, 504)); \
  }
  IBX_TEST_PROTO_REQUESTS_NO_ID(X)
#undef X
  // The start of the session and the two configuration requests do nothing.
#define X(request, message) \
  { \
    client.request(protobuf::message()); \
    expect(fails, #request ": nothing", w.errors.size() == count); \
  }
  IBX_TEST_PROTO_REQUESTS_NOTHING(X)
#undef X
  expect(fails, "every protobuf request was made", count == 78);

  // A message without its id: the id that stands for none.
  size_t before = w.errors.size();
  client.reqMarketDataProtoBuf(protobuf::MarketDataRequest());
  client.placeOrderProtoBuf(protobuf::PlaceOrderRequest());
  expect(fails, "no id", w.errors.size() == before + 2 && w.errors[before] == std::make_pair(-1, 504) && w.errors[before + 1] == std::make_pair(0, 504));

  // What the client has no counterpart for is refused, and nothing is sent.
  before = w.errors.size();
  protobuf::ExerciseOptionsRequest exercise;
  exercise.set_orderid(5);
  exercise.set_manualordertime("20261114 10:00:00");
  client.exerciseOptionsProtoBuf(exercise);
  expect(fails, "exercise with a manual order time", w.errors.size() == before + 1 && w.errors.back() == std::make_pair(5, 321)
                                                       && contains(w.texts.back(), "manualOrderTime"));
  exercise.clear_manualordertime();
  exercise.set_professionalcustomer(true);
  client.exerciseOptionsProtoBuf(exercise);
  expect(fails, "exercise for a professional customer", w.errors.size() == before + 2 && w.errors.back() == std::make_pair(5, 321));
  exercise.set_professionalcustomer(false);
  client.exerciseOptionsProtoBuf(exercise);
  expect(fails, "exercise without them", w.errors.size() == before + 3 && w.errors.back() == std::make_pair(5, 504));
  protobuf::PlaceOrderRequest place;
  place.set_orderid(6);
  place.mutable_order()->add_conditions()->set_type(2);
  client.placeOrderProtoBuf(place);
  expect(fails, "an order condition of an unknown kind", w.errors.size() == before + 4 && w.errors.back() == std::make_pair(6, 321)
                                                           && contains(w.texts.back(), "condition"));
  report("each protobuf request reaches its plain request", fails);
}

// The objects of a request message are the ones of the plain request: what
// reaches the Rust client is the same.
void requestObjects()
{
  Failures fails;

  // A contract with legs and a delta neutral contract.
  protobuf::Contract pc;
  pc.set_conid(265598);
  pc.set_symbol("AAPL");
  pc.set_sectype("OPT");
  pc.set_lasttradedateorcontractmonth("20261218");
  pc.set_strike(150.5);
  pc.set_right("C");
  pc.set_multiplier(100);
  pc.set_exchange("SMART");
  pc.set_primaryexch("NASDAQ");
  pc.set_currency("USD");
  pc.set_localsymbol("AAPL  261218C00150500");
  pc.set_tradingclass("AAPL");
  pc.set_includeexpired(true);
  pc.set_secidtype("ISIN");
  pc.set_secid("US0378331005");
  protobuf::ComboLeg* leg = pc.add_combolegs();
  leg->set_conid(11);
  leg->set_ratio(2);
  leg->set_action("BUY");
  leg->set_exchange("SMART");
  leg->set_perlegprice(1.5);
  leg = pc.add_combolegs();
  leg->set_conid(12);
  leg->set_ratio(1);
  leg->set_action("SELL");
  leg->set_exchange("SMART");
  leg->set_exemptcode(3);
  pc.mutable_deltaneutralcontract()->set_conid(9);
  pc.mutable_deltaneutralcontract()->set_delta(0.5);
  pc.mutable_deltaneutralcontract()->set_price(12.5);

  Contract c;
  c.conId = 265598;
  c.symbol = "AAPL";
  c.secType = "OPT";
  c.lastTradeDateOrContractMonth = "20261218";
  c.strike = 150.5;
  c.right = "C";
  c.multiplier = "100";
  c.exchange = "SMART";
  c.primaryExchange = "NASDAQ";
  c.currency = "USD";
  c.localSymbol = "AAPL  261218C00150500";
  c.tradingClass = "AAPL";
  c.includeExpired = true;
  c.secIdType = "ISIN";
  c.secId = "US0378331005";
  c.comboLegs = std::make_shared<Contract::ComboLegList>();
  c.comboLegs->push_back(std::make_shared<ComboLeg>());
  c.comboLegs->push_back(std::make_shared<ComboLeg>());
  (*c.comboLegs)[0]->conId = 11;
  (*c.comboLegs)[0]->ratio = 2;
  (*c.comboLegs)[0]->action = "BUY";
  (*c.comboLegs)[0]->exchange = "SMART";
  (*c.comboLegs)[1]->conId = 12;
  (*c.comboLegs)[1]->ratio = 1;
  (*c.comboLegs)[1]->action = "SELL";
  (*c.comboLegs)[1]->exchange = "SMART";
  (*c.comboLegs)[1]->exemptCode = 3;
  c.deltaNeutralContract.reset(new DeltaNeutralContract());
  c.deltaNeutralContract->conId = 9;
  c.deltaNeutralContract->delta = 0.5;
  c.deltaNeutralContract->price = 12.5;

  Order legPrices;
  std::string fromMessage = describe("Contract", ibx::pb::contractOf(pc, &legPrices));
  expect(fails, "contract", fromMessage == describe("Contract", c) && contains(fromMessage, "AAPL"));
  expect(fails, "the prices of the legs go to the order", legPrices.orderComboLegs && legPrices.orderComboLegs->size() == 2
                                                            && (*legPrices.orderComboLegs)[0]->price == 1.5
                                                            && (*legPrices.orderComboLegs)[1]->price == UNSET_DOUBLE);
  expect(fails, "an empty contract message is a contract as it starts", describe("Contract", ibx::pb::contractOf(protobuf::Contract())) == describe("Contract", Contract()));

  // An order with its options, conditions, tier and attached orders.
  protobuf::Order po;
  po.set_action("BUY");
  po.set_totalquantity("100");
  po.set_ordertype("LMT");
  po.set_lmtprice(150.25);
  po.set_tif("GTC");
  po.set_transmit(true);
  po.set_outsiderth(true);
  po.set_account("ACCOUNT1");
  po.set_ocagroup("oca");
  po.set_orderref("ref");
  po.set_parentid(3);
  po.set_origin(1);
  po.set_usepricemgmtalgo(1);
  po.set_routemarketabletobbo(1);
  po.set_cashqty(1000.5);
  po.set_algostrategy("Adaptive");
  (*po.mutable_algoparams())["adaptivePriority"] = "Normal";
  (*po.mutable_algoparams())["a"] = "b";
  (*po.mutable_ordermiscoptions())["k"] = "v";
  po.mutable_softdollartier()->set_name("tier");
  po.mutable_softdollartier()->set_value("val");
  po.mutable_softdollartier()->set_displayname("shown");
  protobuf::OrderCondition* pcond = po.add_conditions();
  pcond->set_type(1);
  pcond->set_isconjunctionconnection(true);
  pcond->set_ismore(true);
  pcond->set_conid(11);
  pcond->set_exchange("EX1");
  pcond->set_price(101.5);
  pcond->set_triggermethod(2);
  pcond = po.add_conditions();
  pcond->set_type(3);
  pcond->set_time("20260311-09:30:00");
  protobuf::AttachedOrders attached;
  attached.set_slorderid(41);
  attached.set_slordertype("STP");
  attached.set_ptorderid(42);
  attached.set_ptordertype("LMT");

  Order o;
  o.action = "BUY";
  o.totalQuantity = DecimalFunctions::stringToDecimal("100");
  o.orderType = "LMT";
  o.lmtPrice = 150.25;
  o.tif = "GTC";
  o.outsideRth = true;
  o.account = "ACCOUNT1";
  o.ocaGroup = "oca";
  o.orderRef = "ref";
  o.parentId = 3;
  o.origin = FIRM;
  o.usePriceMgmtAlgo = USE;
  o.routeMarketableToBbo = STATE_YES;
  o.cashQty = 1000.5;
  o.algoStrategy = "Adaptive";
  o.algoParams = std::make_shared<TagValueList>();
  o.algoParams->push_back(std::make_shared<TagValue>("a", "b"));
  o.algoParams->push_back(std::make_shared<TagValue>("adaptivePriority", "Normal"));
  o.orderMiscOptions = std::make_shared<TagValueList>();
  o.orderMiscOptions->push_back(std::make_shared<TagValue>("k", "v"));
  o.softDollarTier = SoftDollarTier("tier", "val", "shown");
  std::shared_ptr<PriceCondition> price(dynamic_cast<PriceCondition*>(OrderCondition::create(OrderCondition::Price)));
  price->conjunctionConnection(true);
  price->isMore(true);
  price->conId(11);
  price->exchange("EX1");
  price->price(101.5);
  price->triggerMethod(2);
  std::shared_ptr<TimeCondition> time(dynamic_cast<TimeCondition*>(OrderCondition::create(OrderCondition::Time)));
  time->conjunctionConnection(true);
  time->time("20260311-09:30:00");
  o.conditions.push_back(price);
  o.conditions.push_back(time);
  o.slOrderId = 41;
  o.slOrderType = "STP";
  o.ptOrderId = 42;
  o.ptOrderType = "LMT";

  Order read;
  std::string refused;
  expect(fails, "order: read", ibx::pb::orderOf(po, &attached, read, refused) && refused.empty());
  fromMessage = describe("Order", read);
  expect(fails, "order", fromMessage == describe("Order", o) && contains(fromMessage, "Adaptive") && !contains(fromMessage, "refused"));

  // An order message leaves `transmit` out for no.
  po.clear_transmit();
  Order held;
  ibx::pb::orderOf(po, nullptr, held, refused);
  expect(fails, "an order without transmit is not transmitted", !held.transmit && contains(describe("Order", held), "transmit: false"));
  // The plain request refuses an order that includes the night in its
  // conditions: its message is refused the same way.
  po.set_conditionsincludeovernight(true);
  Order night;
  ibx::pb::orderOf(po, nullptr, night, refused);
  expect(fails, "conditions over the night", contains(describe("Order", night), "refused 10340"));

  // The other objects of the requests.
  protobuf::ExecutionFilter pf;
  pf.set_clientid(3);
  pf.set_acctcode("ACCOUNT1");
  pf.set_time("20261114 09:30:00");
  pf.set_symbol("AAPL");
  pf.set_sectype("STK");
  pf.set_exchange("SMART");
  pf.set_side("BUY");
  ExecutionFilter filter, plainFilter;
  ibx::pb::copy(pf, filter);
  plainFilter.m_clientId = 3;
  plainFilter.m_acctCode = "ACCOUNT1";
  plainFilter.m_time = "20261114 09:30:00";
  plainFilter.m_symbol = "AAPL";
  plainFilter.m_secType = "STK";
  plainFilter.m_exchange = "SMART";
  plainFilter.m_side = "BUY";
  expect(fails, "execution filter", describe("ExecutionFilter", filter) == describe("ExecutionFilter", plainFilter) && contains(describe("ExecutionFilter", filter), "AAPL"));

  protobuf::ScannerSubscription ps;
  ps.set_numberofrows(25);
  ps.set_instrument("STK");
  ps.set_locationcode("STK.US.MAJOR");
  ps.set_scancode("TOP_PERC_GAIN");
  ps.set_aboveprice(5.5);
  ps.set_abovevolume(100000);
  ps.set_excludeconvertible(true);
  ps.set_stocktypefilter("ALL");
  ScannerSubscription scan, plainScan;
  ibx::pb::copy(ps, scan);
  plainScan.numberOfRows = 25;
  plainScan.instrument = "STK";
  plainScan.locationCode = "STK.US.MAJOR";
  plainScan.scanCode = "TOP_PERC_GAIN";
  plainScan.abovePrice = 5.5;
  plainScan.aboveVolume = 100000;
  plainScan.excludeConvertible = 1;
  plainScan.stockTypeFilter = "ALL";
  expect(fails, "scanner subscription", describe("ScannerSubscription", scan) == describe("ScannerSubscription", plainScan)
                                          && contains(describe("ScannerSubscription", scan), "TOP_PERC_GAIN"));

  protobuf::WshEventDataRequest pw;
  pw.set_reqid(4);
  pw.set_conid(8314);
  pw.set_fillportfolio(true);
  pw.set_startdate("20260101");
  pw.set_enddate("20260201");
  pw.set_totallimit(10);
  WshEventData wsh;
  ibx::pb::copy(pw, wsh);
  expect(fails, "calendar data request", describe("WshEventData", wsh) == describe("WshEventData", WshEventData(8314, false, true, false, "20260101", "20260201", 10)));

  // Every field: an object filled with a value for each of its members,
  // written to its message and read back, reaches the Rust client as the
  // object itself does.
  {
    Contract filled;
    fill(filled, 5);
    // A number the structure keeps as a text.
    filled.multiplier = "100";
    // Only received: the request message of the official client leaves it out.
    filled.lastTradeDate.clear();
    protobuf::Contract p;
    toProto(filled, p, nullptr);
    differences("every field of a contract", describe("Contract", ibx::pb::contractOf(p)), describe("Contract", filled), fails);
  }
  {
    Order filled;
    fill(filled, 5);
    // What an order message has no place for (the id is the request's own;
    // the auction strategy and the basis points are not in the message),
    // and what the official client only receives in a report.
    filled.orderId = 0;
    filled.auctionStrategy = AUCTION_UNSET;
    filled.basisPoints = UNSET_DOUBLE;
    filled.basisPointsType = UNSET_INTEGER;
    filled.autoCancelDate.clear();
    filled.filledQuantity = UNSET_DECIMAL;
    filled.shareholder.clear();
    protobuf::Order p;
    toProto(filled, p);
    toProto(filled.algoParams, *p.mutable_algoparams());
    toProto(filled.smartComboRoutingParams, *p.mutable_smartcomboroutingparams());
    toProto(filled.orderMiscOptions, *p.mutable_ordermiscoptions());
    protobuf::AttachedOrders attachedOrders;
    toProto(filled, attachedOrders);
    Order back;
    ibx::pb::orderOf(p, &attachedOrders, back, refused);
    differences("every field of an order", describe("Order", back), describe("Order", filled), fails);
  }
  {
    ExecutionFilter filled, back;
    fill(filled, 5);
    protobuf::ExecutionFilter p;
    toProto(filled, p);
    ibx::pb::copy(p, back);
    differences("every field of an execution filter", describe("ExecutionFilter", back), describe("ExecutionFilter", filled), fails);
  }
  {
    ScannerSubscription filled, back;
    fill(filled, 5);
    protobuf::ScannerSubscription p;
    toProto(filled, p);
    ibx::pb::copy(p, back);
    differences("every field of a scanner subscription", describe("ScannerSubscription", back), describe("ScannerSubscription", filled), fails);
  }
  {
    WshEventData filled, back;
    fill(filled, 5);
    protobuf::WshEventDataRequest p;
    toProto(filled, p);
    ibx::pb::copy(p, back);
    differences("every field of a calendar data request", describe("WshEventData", back), describe("WshEventData", filled), fails);
  }

  // The options of a request, in the order of their names.
  google::protobuf::Map<std::string, std::string> options;
  options["b"] = "2";
  options["a"] = "1";
  TagValueListSPtr tags = ibx::pb::tagsOf(options);
  expect(fails, "options", tags && tags->size() == 2 && (*tags)[0]->tag == "a" && (*tags)[1]->value == "2"
                             && !ibx::pb::tagsOf(google::protobuf::Map<std::string, std::string>()));
  report("the objects of the request messages", fails);
}

}  // namespace

int main()
{
  everyCallback();
  pricesBarsAndRows();
  values();
  noCostWhenNotUsed();
  requestsReachTheirPlainRequest();
  requestObjects();
  std::printf("%d passed, %d failed\n", passed, failed);
  return failed ? 1 : 0;
}
