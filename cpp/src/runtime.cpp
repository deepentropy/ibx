// The helpers of the generated code, the order conditions, the small
// utilities and the wrapper that does nothing.

#include "ibx_runtime.h"

#include <chrono>
#include <sstream>

#include "DefaultEWrapper.h"
#include "ExecutionCondition.h"
#include "MarginCondition.h"
#include "PercentChangeCondition.h"
#include "PriceCondition.h"
#include "TimeCondition.h"
#include "VolumeCondition.h"

namespace ibx {

// ── Buffers of a thread's requests ──

namespace {
struct Pool {
  std::vector<std::unique_ptr<std::string>> buffers;
  size_t used = 0;
};

Pool& pool()
{
  thread_local Pool p;
  return p;
}

std::string& take()
{
  Pool& p = pool();
  if (p.used == p.buffers.size())
    p.buffers.emplace_back(new std::string());
  std::string& s = *p.buffers[p.used++];
  s.clear();
  return s;
}
}  // namespace

Buf::Buf() : text(take()), w(text) {}

Buf::~Buf() { --pool().used; }

// ── Lists ──

void writeTags(Writer& w, const TagValueListSPtr& list)
{
  w.n(list ? list->size() : 0);
  if (!list)
    return;
  for (const TagValueSPtr& item : *list) {
    w.s(item ? item->tag : std::string());
    w.s(item ? item->value : std::string());
  }
}

void readTags(Reader& r, TagValueListSPtr& list)
{
  size_t count = r.n();
  if (!count)
    return;
  list = std::make_shared<TagValueList>();
  list->reserve(count);
  for (size_t k = 0; k < count; ++k) {
    std::string tag = r.s();
    std::string value = r.s();
    list->push_back(std::make_shared<TagValue>(tag, value));
  }
}

void writeLegPrices(Writer& w, const Order::OrderComboLegListSPtr& legs)
{
  w.n(legs ? legs->size() : 0);
  if (!legs)
    return;
  for (const OrderComboLegSPtr& leg : *legs)
    w.f(leg ? leg->price : UNSET_DOUBLE);
}

void readLegPrices(Reader& r, Order::OrderComboLegListSPtr& legs)
{
  size_t count = r.n();
  if (!count)
    return;
  legs = std::make_shared<Order::OrderComboLegList>();
  legs->reserve(count);
  for (size_t k = 0; k < count; ++k) {
    OrderComboLegSPtr leg = std::make_shared<OrderComboLeg>();
    leg->price = r.f();
    legs->push_back(leg);
  }
}

// A condition crosses as its kind, whether it is joined by "and", then the
// fields every kind may have: above or below, contract, exchange, a
// number, a text, symbol, security type, trigger method.
void writeConditions(Writer& w, const std::vector<std::shared_ptr<OrderCondition>>& conditions)
{
  w.n(conditions.size());
  for (const std::shared_ptr<OrderCondition>& item : conditions) {
    OrderCondition* c = item.get();
    bool isMore = false;
    long long conId = 0;
    long long method = 0;
    double number = 0;
    std::string exchange, text, symbol, secType;
    if (OperatorCondition* op = dynamic_cast<OperatorCondition*>(c))
      isMore = op->isMore();
    if (ContractCondition* cc = dynamic_cast<ContractCondition*>(c)) {
      conId = cc->conId();
      exchange = cc->exchange();
    }
    if (PriceCondition* pc = dynamic_cast<PriceCondition*>(c)) {
      number = pc->price();
      method = pc->triggerMethod();
    } else if (TimeCondition* tc = dynamic_cast<TimeCondition*>(c)) {
      text = tc->time();
    } else if (MarginCondition* mc = dynamic_cast<MarginCondition*>(c)) {
      number = mc->percent();
    } else if (ExecutionCondition* ec = dynamic_cast<ExecutionCondition*>(c)) {
      exchange = ec->exchange();
      symbol = ec->symbol();
      secType = ec->secType();
    } else if (VolumeCondition* vc = dynamic_cast<VolumeCondition*>(c)) {
      number = vc->volume();
    } else if (PercentChangeCondition* pcc = dynamic_cast<PercentChangeCondition*>(c)) {
      number = pcc->changePercent();
    }
    // A missing condition crosses as a kind the client refuses.
    w.i(c ? static_cast<long long>(c->type()) : 0);
    w.b(c ? c->conjunctionConnection() : true);
    w.b(isMore);
    w.i(conId);
    w.s(exchange);
    w.f(number);
    w.s(text);
    w.s(symbol);
    w.s(secType);
    w.i(method);
  }
}

void readConditions(Reader& r, std::vector<std::shared_ptr<OrderCondition>>& conditions)
{
  size_t count = r.n();
  conditions.clear();
  for (size_t k = 0; k < count; ++k) {
    long long kind = r.i();
    bool conjunction = r.b();
    bool isMore = r.b();
    long long conId = r.i();
    std::string exchange = r.s();
    double number = r.f();
    std::string text = r.s();
    std::string symbol = r.s();
    std::string secType = r.s();
    long long method = r.i();
    std::shared_ptr<OrderCondition> c(OrderCondition::create(static_cast<OrderCondition::OrderConditionType>(kind)));
    if (!c)
      continue;
    c->conjunctionConnection(conjunction);
    if (OperatorCondition* op = dynamic_cast<OperatorCondition*>(c.get()))
      op->isMore(isMore);
    if (ContractCondition* cc = dynamic_cast<ContractCondition*>(c.get())) {
      cc->conId(static_cast<int>(conId));
      cc->exchange(exchange);
    }
    if (PriceCondition* pc = dynamic_cast<PriceCondition*>(c.get())) {
      pc->price(number);
      pc->triggerMethod(static_cast<int>(method));
    } else if (TimeCondition* tc = dynamic_cast<TimeCondition*>(c.get())) {
      tc->time(text);
    } else if (MarginCondition* mc = dynamic_cast<MarginCondition*>(c.get())) {
      mc->percent(static_cast<int>(number));
    } else if (ExecutionCondition* ec = dynamic_cast<ExecutionCondition*>(c.get())) {
      ec->exchange(exchange);
      ec->symbol(symbol);
      ec->secType(secType);
    } else if (VolumeCondition* vc = dynamic_cast<VolumeCondition*>(c.get())) {
      vc->volume(static_cast<int>(number));
    } else if (PercentChangeCondition* pcc = dynamic_cast<PercentChangeCondition*>(c.get())) {
      pcc->changePercent(number);
    }
    conditions.push_back(c);
  }
}

// ── Quantities ──

Decimal decimalFromText(const std::string& text)
{
  return text.empty() ? UNSET_DECIMAL : DecimalFunctions::stringToDecimal(text);
}

std::string quantityText(Decimal quantity)
{
  // The exponent of a decimal64: ten bits above the coefficient, biased by
  // 398; the form with the two top bits of the field set has it two bits
  // higher and never occurs for a quantity.
  std::uint64_t bits = quantity.bits();
  if (quantity.isUnset() || ((bits >> 61) & 3) == 3)
    return std::string();
  int exponent = static_cast<int>((bits >> 53) & 0x3FF) - 398;
  return exponent < -8 ? DecimalFunctions::decimalStringToDisplay(quantity) : std::string();
}

// ── Decoders of the callbacks ──

std::set<std::string> decTextSet(Reader& r)
{
  std::set<std::string> out;
  for (size_t k = r.n(); k > 0; --k)
    out.insert(r.s());
  return out;
}

std::set<double> decDoubleSet(Reader& r)
{
  std::set<double> out;
  for (size_t k = r.n(); k > 0; --k)
    out.insert(r.f());
  return out;
}

std::vector<SoftDollarTier> decSoftDollarTiers(Reader& r)
{
  std::vector<SoftDollarTier> out;
  for (size_t k = r.n(); k > 0; --k) {
    std::string name = r.s();
    std::string val = r.s();
    std::string displayName = r.s();
    out.push_back(SoftDollarTier(name, val, displayName));
  }
  return out;
}

std::vector<ContractDescription> decContractDescriptions(Reader& r)
{
  std::vector<ContractDescription> out(r.n());
  for (ContractDescription& d : out) {
    d.contract.conId = static_cast<int>(r.i());
    d.contract.symbol = r.s();
    d.contract.secType = r.s();
    d.contract.primaryExchange = r.s();
    d.contract.currency = r.s();
    d.contract.description = r.s();
    d.contract.issuerId = r.s();
    for (size_t k = r.n(); k > 0; --k)
      d.derivativeSecTypes.push_back(r.s());
  }
  return out;
}

SmartComponentsMap decSmartComponents(Reader& r)
{
  SmartComponentsMap out;
  for (size_t k = r.n(); k > 0; --k) {
    int bit = static_cast<int>(r.i());
    std::string exchange = r.s();
    std::string letter = r.s();
    out[bit] = std::make_tuple(exchange, letter.empty() ? '\0' : letter[0]);
  }
  return out;
}

HistogramDataVector decHistogram(Reader& r)
{
  HistogramDataVector out(r.n());
  for (HistogramEntry& e : out) {
    e.price = r.f();
    e.size = decimalFromDouble(r.f());
  }
  return out;
}

std::vector<HistoricalSession> decSessions(Reader& r)
{
  std::vector<HistoricalSession> out(r.n());
  for (HistoricalSession& s : out) {
    s.startDateTime = r.s();
    s.endDateTime = r.s();
    s.refDate = r.s();
  }
  return out;
}

std::vector<HistoricalTick> decTicks(Reader& r)
{
  std::vector<HistoricalTick> out(r.n());
  for (HistoricalTick& t : out) {
    t.time = r.i();
    t.price = r.f();
    t.size = decimalFromDouble(r.f());
  }
  return out;
}

std::vector<HistoricalTickBidAsk> decTicksBidAsk(Reader& r)
{
  std::vector<HistoricalTickBidAsk> out(r.n());
  for (HistoricalTickBidAsk& t : out) {
    t.time = r.i();
    t.tickAttribBidAsk = tick_attrib_bid_ask(static_cast<std::uint32_t>(r.i()));
    t.priceBid = r.f();
    t.priceAsk = r.f();
    t.sizeBid = decimalFromDouble(r.f());
    t.sizeAsk = decimalFromDouble(r.f());
  }
  return out;
}

std::vector<HistoricalTickLast> decTicksLast(Reader& r)
{
  std::vector<HistoricalTickLast> out(r.n());
  for (HistoricalTickLast& t : out) {
    t.time = r.i();
    t.tickAttribLast = tick_attrib_last(static_cast<std::uint32_t>(r.i()));
    t.price = r.f();
    t.size = decimalFromDouble(r.f());
    t.exchange = r.s();
    t.specialConditions = r.s();
  }
  return out;
}

}  // namespace ibx

// ── Order conditions ──

std::string OrderCondition::toString() { return conjunctionConnection() ? "<AND>" : "<OR>"; }

bool OrderCondition::conjunctionConnection() const { return m_isConjunctionConnection; }

void OrderCondition::conjunctionConnection(bool isConjunctionConnection) { m_isConjunctionConnection = isConjunctionConnection; }

OrderCondition::OrderConditionType OrderCondition::type() { return m_type; }

std::string OrderCondition::typeName()
{
  switch (m_type) {
    case Execution:
      return "Execution";
    case Margin:
      return "Margin";
    case PercentChange:
      return "PercentChange";
    case Price:
      return "Price";
    case Time:
      return "Time";
    case Volume:
      return "Volume";
  }
  return "";
}

OrderCondition* OrderCondition::create(OrderConditionType type)
{
  OrderCondition* made = 0;
  switch (type) {
    case Execution:
      made = new ExecutionCondition();
      break;
    case Margin:
      made = new MarginCondition();
      break;
    case PercentChange:
      made = new PercentChangeCondition();
      break;
    case Price:
      made = new PriceCondition();
      break;
    case Time:
      made = new TimeCondition();
      break;
    case Volume:
      made = new VolumeCondition();
      break;
  }
  if (made)
    made->m_type = type;
  return made;
}

std::string OperatorCondition::toString() { return " is " + std::string(isMore() ? ">= " : "<= ") + valueToString(); }

bool OperatorCondition::isMore() { return m_isMore; }

void OperatorCondition::isMore(bool isMore) { m_isMore = isMore; }

std::string ContractCondition::toString() { return typeName() + " of " + std::to_string(conId()) + OperatorCondition::toString(); }

int ContractCondition::conId() { return m_conId; }

void ContractCondition::conId(int conId) { m_conId = conId; }

std::string ContractCondition::exchange() { return m_exchange; }

void ContractCondition::exchange(const std::string& exchange) { m_exchange = exchange; }

namespace {
template <class T> std::string textOf(const T& value)
{
  std::stringstream s;
  s << value;
  return s.str();
}

template <class T> void valueOf(const std::string& text, T& value)
{
  std::stringstream s;
  s << text;
  s >> value;
}
}  // namespace

std::string PriceCondition::valueToString() const { return textOf(m_price); }

void PriceCondition::valueFromString(const std::string& v) { valueOf(v, m_price); }

double PriceCondition::price() { return m_price; }

void PriceCondition::price(double price) { m_price = price; }

std::string PriceCondition::toString() { return strTriggerMethod() + " " + ContractCondition::toString(); }

PriceCondition::Method PriceCondition::triggerMethod() { return static_cast<Method>(m_triggerMethod); }

std::string PriceCondition::strTriggerMethod()
{
  static const char* const names[] = {"default", "double bid/ask", "last", "double last", "bid/ask", "", "", "last of bid/ask", "mid-point"};
  int k = m_triggerMethod;
  return k < 0 || k >= static_cast<int>(sizeof names / sizeof names[0]) ? "" : names[k];
}

void PriceCondition::triggerMethod(Method triggerMethod) { m_triggerMethod = triggerMethod; }

void PriceCondition::triggerMethod(int triggerMethod) { m_triggerMethod = triggerMethod; }

std::string TimeCondition::valueToString() const { return m_time; }

void TimeCondition::valueFromString(const std::string& v) { m_time = v; }

std::string TimeCondition::toString() { return "time" + OperatorCondition::toString(); }

std::string TimeCondition::time() { return m_time; }

void TimeCondition::time(const std::string& time) { m_time = time; }

std::string MarginCondition::valueToString() const { return textOf(m_percent); }

void MarginCondition::valueFromString(const std::string& v) { valueOf(v, m_percent); }

std::string MarginCondition::toString() { return "the margin cushion percent" + OperatorCondition::toString(); }

int MarginCondition::percent() { return m_percent; }

void MarginCondition::percent(int percent) { m_percent = percent; }

std::string ExecutionCondition::toString()
{
  return "trade occurs for " + m_symbol + " symbol on " + m_exchange + " exchange for " + m_secType + " security type";
}

std::string ExecutionCondition::exchange() { return m_exchange; }

void ExecutionCondition::exchange(const std::string& exchange) { m_exchange = exchange; }

std::string ExecutionCondition::secType() { return m_secType; }

void ExecutionCondition::secType(const std::string& secType) { m_secType = secType; }

std::string ExecutionCondition::symbol() { return m_symbol; }

void ExecutionCondition::symbol(const std::string& symbol) { m_symbol = symbol; }

std::string PercentChangeCondition::valueToString() const { return textOf(m_changePercent); }

void PercentChangeCondition::valueFromString(const std::string& v) { valueOf(v, m_changePercent); }

double PercentChangeCondition::changePercent() { return m_changePercent; }

void PercentChangeCondition::changePercent(double changePercent) { m_changePercent = changePercent; }

std::string VolumeCondition::valueToString() const { return textOf(m_volume); }

void VolumeCondition::valueFromString(const std::string& v) { valueOf(v, m_volume); }

int VolumeCondition::volume() { return m_volume; }

void VolumeCondition::volume(int volume) { m_volume = volume; }

// ── Utilities ──

bool Utils::stringIsEmpty(std::string str) { return str.empty(); }

bool Utils::isValidValue(int value) { return value != UNSET_INTEGER; }

bool Utils::isValidValue(long long value) { return value != UNSET_LLONG; }

bool Utils::isValidValue(double value) { return value != UNSET_DOUBLE; }

bool Utils::isPegBenchOrder(std::string orderType) { return orderType == "PEG BENCH" || orderType == "PEGBENCH"; }

bool Utils::isPegMidOrder(std::string orderType) { return orderType == "PEG MID" || orderType == "PEGMID"; }

bool Utils::isPegBestOrder(std::string orderType) { return orderType == "PEG BEST" || orderType == "PEGBEST"; }

FundDistributionPolicyIndicator Utils::getFundDistributionPolicyIndicator(std::string value)
{
  if (value == "N")
    return FundDistributionPolicyIndicator::AccumulationFund;
  if (value == "Y")
    return FundDistributionPolicyIndicator::IncomeFund;
  return FundDistributionPolicyIndicator::None;
}

FundAssetType Utils::getFundAssetType(std::string value)
{
  static const char* const codes[] = {"000", "001", "002", "003", "004", "005", "006", "007"};
  for (int k = 0; k < 8; ++k) {
    if (value == codes[k])
      return static_cast<FundAssetType>(k + 1);
  }
  return FundAssetType::None;
}

long long Utils::currentTimeMillis()
{
  using namespace std::chrono;
  return duration_cast<milliseconds>(system_clock::now().time_since_epoch()).count();
}

OptionExerciseType Utils::getOptionExerciseType(int val)
{
  switch (val) {
    case 1:
      return OptionExerciseType::Exercise;
    case 2:
      return OptionExerciseType::Lapse;
    case 3:
      return OptionExerciseType::DoNothing;
    case 100:
      return OptionExerciseType::Assigned;
    case 101:
      return OptionExerciseType::AutoexerciseClearing;
    case 102:
      return OptionExerciseType::Expired;
    case 103:
      return OptionExerciseType::Netting;
    case 200:
      return OptionExerciseType::AutoexerciseTrading;
  }
  return OptionExerciseType::None;
}

// ── The wrapper that does nothing ──
// Its protobuf callbacks are in proto_generated.cpp.

#define IBX_EMPTY_CALLBACK(name, args) void DefaultEWrapper::name args {}
#if defined(_MSC_VER)
#pragma warning(push)
#pragma warning(disable : 4100)  // the parameters are named and not used
#elif defined(__GNUC__)
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wunused-parameter"
#endif
EWrapper_methods_regular(IBX_EMPTY_CALLBACK, EWrapper_methods_def_value)
#if defined(_MSC_VER)
#pragma warning(pop)
#elif defined(__GNUC__)
#pragma GCC diagnostic pop
#endif
#undef IBX_EMPTY_CALLBACK
