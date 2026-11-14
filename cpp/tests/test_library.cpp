// The offline tests of the library: every field of every structure and
// every parameter of every callback crosses the C entry layer to the right
// place, the fields with a rule of their own, the classes, and what a
// client that is not connected answers.
//
// Needs the Rust client built with its test helpers (IBX_BUILD_TESTS).

#include <chrono>
#include <cmath>
#include <functional>
#include <stdexcept>

#include "DefaultEWrapper.h"
#include "EClientSocket.h"
#include "EReader.h"
#include "EReaderOSSignal.h"
#include "ExecutionCondition.h"
#include "MarginCondition.h"
#include "OrderStatus.h"
#include "PercentChangeCondition.h"
#include "PriceCondition.h"
#include "TimeCondition.h"
#include "TwsSocketClientErrors.h"
#include "VolumeCondition.h"
#include "test_support.h"

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

template <class T> T sample(const char* kind)
{
  std::string data;
  T x;
  if (ibx_test_sample(ibx::view(kind), &takeText, &data)) {
    ibx::Reader r(ibx::view(data));
    ibx::read(r, x);
  }
  return x;
}

// ── Every field of every structure ──

void structuresToRust()
{
#define X(T) \
  { \
    T x; \
    fill(x, 5); \
    ibx::Buf b; \
    ibx::write(b.w, x); \
    std::string text; \
    Failures fails; \
    expect(fails, "kind " #T, ibx_test_check(ibx::view(#T), b.view(), 5, &takeText, &text)); \
    if (!text.empty()) \
      fails.push_back(text); \
    report("to Rust: " #T, fails); \
  }
  IBX_TEST_IN_STRUCTS(X)
#undef X
}

void structuresFromRust()
{
#define X(T) \
  { \
    std::string data; \
    Failures fails; \
    expect(fails, "kind " #T, ibx_test_make(ibx::view(#T), 7, &takeText, &data)); \
    T x; \
    ibx::Reader r(ibx::view(data)); \
    ibx::read(r, x); \
    check(x, 7, #T, fails); \
    report("from Rust: " #T, fails); \
  }
  IBX_TEST_OUT_STRUCTS(X)
#undef X
}

// ── Every parameter of every callback ──

void callbacks()
{
  CheckWrapper w;
  ibx::Sink sink{&w, nullptr};
  ibx_test_fire(&ibx::callbackTable, &sink);
  Failures fails = w.fails;
  expect(fails, "an exception was kept", !sink.error);
  for (const char* name : expectedCallbacks)
    expect(fails, std::string("called once: ") + name, w.hits.count(name) == 1);
  expect(fails, "no other callback", w.hits.size() == sizeof expectedCallbacks / sizeof expectedCallbacks[0]);
  report("callbacks", fails);
}

// An exception a callback throws is kept and the other callbacks still come.
void callbackException()
{
  struct Throwing : CheckWrapper {
    void tickSize(int, TickType, Decimal) override
    {
      hit("tickSize");
      throw std::runtime_error("from a callback");
    }
  } w;
  ibx::Sink sink{&w, nullptr};
  ibx_test_fire(&ibx::callbackTable, &sink);
  Failures fails;
  expect(fails, "the exception is kept", bool(sink.error));
  expect(fails, "the callbacks after it were given", w.hits.count("currentTimeInMillis") == 1 && w.hits.count("tickSize") == 1);
  bool thrown = false;
  try {
    if (sink.error)
      std::rethrow_exception(sink.error);
  } catch (const std::runtime_error& e) {
    thrown = std::string(e.what()) == "from a callback";
  }
  expect(fails, "it is the callback's exception", thrown);
  report("exception of a callback", fails);
}

// ── The fields with a rule of their own, from Rust ──

void samplesFromRust()
{
  Failures fails;
  Order o = sample<Order>("Order");
  expect(fails, "totalQuantity", DecimalFunctions::decimalStringToDisplay(o.totalQuantity) == "12.5");
  expect(fails, "origin", o.origin == FIRM);
  expect(fails, "usePriceMgmtAlgo", o.usePriceMgmtAlgo == USE);
  expect(fails, "routeMarketableToBbo", o.routeMarketableToBbo == STATE_YES);
  expect(fails, "seekPriceImprovement", o.seekPriceImprovement == STATE_NO);
  expect(fails, "softDollarTier", o.softDollarTier.name() == "tier" && o.softDollarTier.val() == "val" && o.softDollarTier.displayName() == "shown");
  expect(fails, "orderComboLegs", o.orderComboLegs && o.orderComboLegs->size() == 2 && (*o.orderComboLegs)[0]->price == 1.5
                                   && (*o.orderComboLegs)[1]->price == UNSET_DOUBLE);
  // "None" is 0 in the client and the unset value in the structure; the
  // trailing unit is 0 for none, as a report of the reference carries it.
  expect(fails, "values that are set", o.referenceContractId == 7 && o.parentPermId == 99 && o.adjustableTrailingUnit == 0);
  expect(fails, "none is unset", o.refFuturesConId == UNSET_INTEGER && o.peggedChangeAmount == UNSET_DOUBLE && o.referenceChangeAmount == UNSET_DOUBLE);
  expect(fails, "conditions", o.conditions.size() == 6);
  if (o.conditions.size() == 6) {
    PriceCondition* price = dynamic_cast<PriceCondition*>(o.conditions[0].get());
    expect(fails, "price condition", price && price->conId() == 11 && price->exchange() == "EX1" && price->price() == 101.5 && price->isMore()
                                       && price->triggerMethod() == PriceCondition::Last && price->conjunctionConnection());
    TimeCondition* time = dynamic_cast<TimeCondition*>(o.conditions[1].get());
    expect(fails, "time condition", time && time->time() == "20260311-09:30:00" && !time->isMore());
    MarginCondition* margin = dynamic_cast<MarginCondition*>(o.conditions[2].get());
    expect(fails, "margin condition", margin && margin->percent() == 30 && margin->isMore());
    ExecutionCondition* execution = dynamic_cast<ExecutionCondition*>(o.conditions[3].get());
    expect(fails, "execution condition", execution && execution->symbol() == "SYM" && execution->exchange() == "EX2" && execution->secType() == "STK");
    VolumeCondition* volume = dynamic_cast<VolumeCondition*>(o.conditions[4].get());
    expect(fails, "volume condition", volume && volume->conId() == 12 && volume->exchange() == "EX3" && volume->volume() == 5000 && volume->isMore());
    PercentChangeCondition* change = dynamic_cast<PercentChangeCondition*>(o.conditions[5].get());
    expect(fails, "percent change condition", change && change->conId() == 13 && change->exchange() == "EX4" && change->changePercent() == 2.5 && !change->isMore());
  }

  Order exact = sample<Order>("OrderExact");
  expect(fails, "a quantity of twelve decimals", DecimalFunctions::decimalStringToDisplay(exact.totalQuantity) == "0.123456789012");

  Order unset = sample<Order>("OrderUnset");
  expect(fails, "unset quantity", unset.totalQuantity.isUnset() && unset.filledQuantity.isUnset());
  expect(fails, "unset numbers", unset.lmtPrice == UNSET_DOUBLE && unset.minQty == UNSET_INTEGER && unset.usePriceMgmtAlgo == DEFAULT);
  expect(fails, "none of the client is unset here", unset.referenceContractId == UNSET_INTEGER && unset.refFuturesConId == UNSET_INTEGER
                                                      && unset.parentPermId == UNSET_LLONG && unset.peggedChangeAmount == UNSET_DOUBLE
                                                      && unset.referenceChangeAmount == UNSET_DOUBLE && unset.adjustableTrailingUnit == 0);
  expect(fails, "empty lists are null", !unset.algoParams && !unset.orderComboLegs && !unset.orderMiscOptions && unset.conditions.empty());

  Contract c = sample<Contract>("Contract");
  expect(fails, "strike as given", c.strike == 0.0);
  expect(fails, "no legs, no delta neutral contract", !c.comboLegs && !c.deltaNeutralContract);

  ContractDetails d = sample<ContractDetails>("ContractDetails");
  expect(fails, "fund policy", d.fundDistributionPolicyIndicator == FundDistributionPolicyIndicator::IncomeFund);
  expect(fails, "fund asset type", d.fundAssetType == FundAssetType::Equity);
  expect(fails, "no list when empty", !d.secIdList && !d.ineligibilityReasonList);

  CommissionAndFeesReport report_ = sample<CommissionAndFeesReport>("CommissionAndFeesReport");
  expect(fails, "yield redemption date", report_.yieldRedemptionDate == 20270115);

  Bar bar = sample<Bar>("Bar");
  expect(fails, "bar volume", DecimalFunctions::decimalStringToDisplay(bar.volume) == "1234");
  report("fields with a rule of their own, from Rust", fails);
}

// ── The fields with a rule of their own, to Rust ──

void samplesToRust()
{
  Failures fails;
  Contract c;
  expect(fails, "unset strike is 0", contains(describe("Contract", c), "strike: 0.0"));
  c.strike = 12.5;
  c.deltaNeutralContract.reset(new DeltaNeutralContract());
  c.deltaNeutralContract->conId = 9;
  c.deltaNeutralContract->delta = 0.5;
  std::string text = describe("Contract", c);
  expect(fails, "strike", contains(text, "strike: 12.5"));
  expect(fails, "delta neutral contract", contains(text, "delta_neutral_contract: Some(DeltaNeutralContract { con_id: 9, delta: 0.5"));

  Order o;
  text = describe("Order", o);
  expect(fails, "default order: unset quantity", contains(text, "total_quantity: 1.7976931348623157e308, total_quantity_text: \"\""));
  expect(fails, "default order: flags", contains(text, "route_marketable_to_bbo: false") && contains(text, "seek_price_improvement: false")
                                          && contains(text, "use_price_mgmt_algo: 2147483647") && contains(text, "origin: 0"));
  expect(fails, "default order: not refused", !contains(text, "refused"));
  // The unset values of the structure are the client's "none".
  expect(fails, "default order: none", contains(text, "reference_contract_id: 0,") && contains(text, "ref_futures_con_id: 0,")
                                         && contains(text, "parent_perm_id: 0,") && contains(text, "pegged_change_amount: 0.0,")
                                         && contains(text, "reference_change_amount: 0.0,") && contains(text, "adjustable_trailing_unit: 0,"));

  o.totalQuantity = DecimalFunctions::stringToDecimal("100");
  o.referenceContractId = 7;
  o.peggedChangeAmount = 0.5;
  o.parentPermId = 99;
  o.adjustableTrailingUnit = 100;
  o.origin = FIRM;
  o.usePriceMgmtAlgo = USE;
  o.routeMarketableToBbo = STATE_YES;
  o.seekPriceImprovement = STATE_NO;
  o.softDollarTier = SoftDollarTier("tier", "val", "shown");
  o.orderComboLegs = std::make_shared<Order::OrderComboLegList>();
  o.orderComboLegs->push_back(std::make_shared<OrderComboLeg>());
  o.orderComboLegs->push_back(std::make_shared<OrderComboLeg>());
  (*o.orderComboLegs)[0]->price = 1.5;
  std::shared_ptr<PriceCondition> price(dynamic_cast<PriceCondition*>(OrderCondition::create(OrderCondition::Price)));
  price->conId(11);
  price->exchange("EX1");
  price->price(101.5);
  price->isMore(true);
  price->triggerMethod(PriceCondition::Last);
  price->conjunctionConnection(true);
  std::shared_ptr<TimeCondition> time(dynamic_cast<TimeCondition*>(OrderCondition::create(OrderCondition::Time)));
  time->time("20260311-09:30:00");
  o.conditions.push_back(price);
  o.conditions.push_back(time);
  text = describe("Order", o);
  expect(fails, "quantity", contains(text, "total_quantity: 100.0, total_quantity_text: \"\""));
  expect(fails, "origin", contains(text, "origin: 1"));
  expect(fails, "values that are set", contains(text, "reference_contract_id: 7,") && contains(text, "pegged_change_amount: 0.5,")
                                         && contains(text, "parent_perm_id: 99,") && contains(text, "adjustable_trailing_unit: 100,"));
  expect(fails, "price management", contains(text, "use_price_mgmt_algo: 1"));
  expect(fails, "three state flags", contains(text, "route_marketable_to_bbo: true") && contains(text, "seek_price_improvement: false"));
  expect(fails, "soft dollar tier", contains(text, "soft_dollar_tier_name: \"tier\", soft_dollar_tier_val: \"val\", soft_dollar_tier_display_name: \"shown\""));
  expect(fails, "leg prices", contains(text, "order_combo_legs: [1.5, 1.7976931348623157e308]"));
  expect(fails, "conditions", contains(text, "Price { con_id: 11, exchange: \"EX1\", price: 10150000000, is_more: true, trigger_method: 2 }")
                                && contains(text, "Time { time: \"20260311-09:30:00\", is_more: false }"));
  expect(fails, "conditions joined by and: not refused", !contains(text, "refused"));

  // A quantity the number alone cannot hold goes with its text.
  o.totalQuantity = DecimalFunctions::stringToDecimal("0.123456789012");
  expect(fails, "exact quantity", contains(describe("Order", o), "total_quantity_text: \"0.123456789012\""));

  // What the client cannot send refuses the order.
  price->conjunctionConnection(false);
  expect(fails, "conditions joined by or", contains(describe("Order", o), "refused 321"));
  price->conjunctionConnection(true);
  o.conditionsIncludeOvernight = true;
  expect(fails, "conditions over the night", contains(describe("Order", o), "refused 10340"));

  ScannerSubscription s;
  expect(fails, "convertibles: unset", contains(describe("ScannerSubscription", s), "exclude_convertible: false"));
  s.excludeConvertible = 1;
  expect(fails, "convertibles: excluded", contains(describe("ScannerSubscription", s), "exclude_convertible: true"));
  report("fields with a rule of their own, to Rust", fails);
}

// ── The classes ──

void classes()
{
  Failures fails;
  // Quantities.
  Decimal hundred = DecimalFunctions::stringToDecimal("100");
  expect(fails, "decimal text", DecimalFunctions::decimalStringToDisplay(hundred) == "100");
  expect(fails, "decimal to double", DecimalFunctions::decimalToDouble(hundred) == 100.0);
  expect(fails, "decimal sum", DecimalFunctions::decimalStringToDisplay(DecimalFunctions::add(hundred, DecimalFunctions::stringToDecimal("0.5"))) == "100.5");
  expect(fails, "decimal from a double", DecimalFunctions::decimalStringToDisplay(ibx::decimalFromDouble(1.5)) == "1.5");
  expect(fails, "unset decimal", UNSET_DECIMAL.isUnset() && ibx::decimalFromDouble(UNSET_DOUBLE).isUnset()
                                   && ibx::decimalToDoubleOrMax(UNSET_DECIMAL) == UNSET_DOUBLE && !ZERO_DECIMAL.isUnset());
  expect(fails, "quantity text only past eight decimals", ibx::quantityText(hundred).empty()
                                                            && ibx::quantityText(DecimalFunctions::stringToDecimal("0.12345678")).empty()
                                                            && ibx::quantityText(DecimalFunctions::stringToDecimal("0.123456789")) == "0.123456789");

  // Conditions.
  std::unique_ptr<OrderCondition> made(OrderCondition::create(OrderCondition::Volume));
  expect(fails, "condition kind", made && made->type() == OrderCondition::Volume && made->typeName() == "Volume");
  VolumeCondition* volume = dynamic_cast<VolumeCondition*>(made.get());
  volume->conId(8314);
  volume->volume(5000);
  volume->isMore(true);
  expect(fails, "volume condition text", volume->toString() == "Volume of 8314 is >= 5000");
  PriceCondition price;
  expect(fails, "price condition text", price.strTriggerMethod() == "default");
  TimeCondition time;
  time.time("20260311-09:30:00");
  expect(fails, "time condition text", time.toString() == "time is <= 20260311-09:30:00");
  MarginCondition margin;
  margin.percent(30);
  margin.isMore(true);
  expect(fails, "margin condition text", margin.toString() == "the margin cushion percent is >= 30");
  ExecutionCondition execution;
  execution.symbol("SYM");
  execution.exchange("EX");
  execution.secType("STK");
  expect(fails, "execution condition text", execution.toString() == "trade occurs for SYM symbol on EX exchange for STK security type");
  expect(fails, "condition link text", OrderCondition().toString() == "<OR>");

  // Structures as they start.
  Contract c;
  expect(fails, "contract defaults", c.conId == 0 && c.strike == UNSET_DOUBLE && !c.includeExpired && !c.comboLegs && !c.deltaNeutralContract);
  c.deltaNeutralContract.reset(new DeltaNeutralContract());
  c.deltaNeutralContract->conId = 5;
  Contract copy = c;
  expect(fails, "a contract is copied with its delta neutral contract",
         copy.deltaNeutralContract && copy.deltaNeutralContract.get() != c.deltaNeutralContract.get() && copy.deltaNeutralContract->conId == 5);
  Order o;
  expect(fails, "order defaults", o.transmit && o.totalQuantity.isUnset() && o.lmtPrice == UNSET_DOUBLE && o.exemptCode == -1 && o.origin == CUSTOMER
                                    && o.routeMarketableToBbo == STATE_DEFAULT && o.usePriceMgmtAlgo == DEFAULT && o.parentPermId == UNSET_LLONG);
  OrderState state;
  expect(fails, "order state defaults", state.commissionAndFees == UNSET_DOUBLE && state.suggestedSize.isUnset() && !state.orderAllocations);
  Execution e;
  expect(fails, "execution defaults", e.shares.bits() == ZERO_DECIMAL.bits() && e.optExerciseOrLapseType == OptionExerciseType::None);
  ScannerSubscription sub;
  expect(fails, "scanner defaults", sub.numberOfRows == -1 && sub.abovePrice == DBL_MAX && sub.excludeConvertible == INT_MAX);
  WshEventData wsh(8314, false, true, false, "20260101", "20260201", 10);
  expect(fails, "calendar data request", wsh.conId == 8314 && wsh.filter.empty() && wsh.fillPortfolio && wsh.totalLimit == 10);
  ComboLeg leg;
  expect(fails, "combo leg", leg.exemptCode == -1 && leg == ComboLeg());

  // Small helpers.
  expect(fails, "order status", OrderStatusUtil::get("presubmitted") == OrderStatus::PreSubmitted && OrderStatusUtil::isActive(OrderStatus::Submitted)
                                  && OrderStatusUtil::isTerminal(OrderStatus::Filled) && std::string(OrderStatusUtil::toString(OrderStatus::Inactive)) == "Inactive"
                                  && OrderStatusUtil::get("?") == OrderStatus::Unknown);
  expect(fails, "values that are set", Utils::isValidValue(1) && !Utils::isValidValue(UNSET_INTEGER) && !Utils::isValidValue(UNSET_DOUBLE)
                                         && !Utils::isValidValue(UNSET_DECIMAL) && Utils::isValidValue(hundred));
  expect(fails, "fund codes", Utils::getFundAssetType("000") == FundAssetType::Others && Utils::getFundAssetType("007") == FundAssetType::Alternative
                                && Utils::getFundAssetType("x") == FundAssetType::None
                                && Utils::getFundDistributionPolicyIndicator("N") == FundDistributionPolicyIndicator::AccumulationFund);
  expect(fails, "exercise kinds", Utils::getOptionExerciseType(2) == OptionExerciseType::Lapse && Utils::getOptionExerciseType(200) == OptionExerciseType::AutoexerciseTrading
                                    && Utils::getOptionExerciseType(-1) == OptionExerciseType::None);
  expect(fails, "tick kinds", BID_SIZE == 0 && LAST == 4 && RT_VOLUME == 48 && HALTED == 49 && LAST_EXCH == 84 && isPrice(DELAYED_BID) && !isPrice(VOLUME));
  expect(fails, "account data kinds", std::string(faDataTypeStr(GROUPS)) == "GROUPS" && ALIASES == 3);
  report("classes", fails);
}

// ── A client that is not connected ──

void notConnected()
{
  struct Errors : DefaultEWrapper {
    std::vector<std::pair<int, int>> errors;
    void error(int id, long long, int errorCode, const std::string&, const std::string&) override { errors.push_back({id, errorCode}); }
  } w;
  EReaderOSSignal signal(10);
  EClientSocket client(&w, &signal);
  Failures fails;
  expect(fails, "not connected", !client.isConnected() && client.connState() == EClient::CS_DISCONNECTED && client.serverVersion() == 0);
  client.reqCurrentTime();
  client.reqMktData(7, Contract(), "", false, false, TagValueListSPtr());
  client.placeOrder(9, Contract(), Order());
  expect(fails, "error 504 for each request", w.errors.size() == 3 && w.errors[0] == std::make_pair(-1, 504) && w.errors[1] == std::make_pair(7, 504)
                                                && w.errors[2] == std::make_pair(9, 504));
  // The reader and the signal of a client without a session do nothing.
  EReader reader(&client, &signal);
  reader.start();
  signal.waitForSignal();
  reader.processMsgs();
  client.eDisconnect();
  expect(fails, "nothing more", w.errors.size() == 3);
  report("a client that is not connected", fails);
}

// What a market data callback costs through the layer: the Rust wrapper
// call, the C entry, the C++ virtual call (--bench).
void benchTicks()
{
  struct Counter : DefaultEWrapper {
    unsigned long long ticks = 0;
    double sum = 0;
    void tickPrice(int, TickType, double price, const TickAttrib& attrib) override
    {
      ++ticks;
      sum += price + attrib.canAutoExecute;
    }
    void tickSize(int, TickType, Decimal size) override
    {
      ++ticks;
      sum += static_cast<double>(size.bits() & 1);
    }
  } w;
  ibx::Sink sink{&w, nullptr};
  const unsigned long long count = 20000000;
  ibx_test_ticks(&ibx::callbackTable, &sink, 100000);  // warm up
  w.ticks = 0;
  auto start = std::chrono::steady_clock::now();
  ibx_test_ticks(&ibx::callbackTable, &sink, count);
  double ns = std::chrono::duration<double, std::nano>(std::chrono::steady_clock::now() - start).count();
  std::printf("%llu callbacks (tickPrice, tickSize), %.2f ns each (sum %.0f)\n", w.ticks, ns / static_cast<double>(w.ticks), w.sum);
}

}  // namespace

int runReplays();

int main(int argc, char** argv)
{
  if (argc >= 2 && std::string(argv[1]) == "--bench") {
    benchTicks();
    return 0;
  }
  bool replays = argc < 2 || std::string(argv[1]) != "--no-replay";
  structuresToRust();
  structuresFromRust();
  callbacks();
  callbackException();
  samplesFromRust();
  samplesToRust();
  classes();
  notConnected();
  int replayFailures = replays ? runReplays() : 0;
  std::printf("%d passed, %d failed, %d replays failed\n", passed, failed, replayFailures);
  return failed || replayFailures ? 1 : 0;
}
