// The protobuf forms (IBX_PROTOBUF), the part written by hand: the values a
// message carries, the messages made of several objects, the callbacks
// whose message does not follow its plain callback one for one, and the
// requests whose message is not read field by field.
//
// The rules are the ones of the Python client's protobuf layer
// (src/python/compat/proto_forms.py): a program gets the same messages
// from both.

#include "ibx_proto.h"

#include <algorithm>
#include <cctype>
#include <charconv>
#include <cmath>
#include <cstdio>
#include <cstdlib>

#include "ExecutionCondition.h"
#include "MarginCondition.h"
#include "PercentChangeCondition.h"
#include "PriceCondition.h"
#include "TimeCondition.h"
#include "VolumeCondition.h"

namespace ibx {

unsigned long long protoMessagesBuilt = 0;

namespace pb {

// ── Values as a message carries them ──

std::string floatText(double v)
{
  if (std::isnan(v))
    return "nan";
  if (std::isinf(v))
    return v > 0 ? "inf" : "-inf";
  // The shortest digits, as d.ddde±xx.
  char text[40];
  std::to_chars_result made = std::to_chars(text, text + sizeof text, v, std::chars_format::scientific);
  std::string s(text, made.ptr);
  size_t e = s.find('e');
  int exponent = std::atoi(s.c_str() + e + 1);
  std::string sign = s[0] == '-' ? "-" : "";
  std::string digits;
  for (size_t k = sign.size(); k < e; ++k) {
    if (s[k] != '.')
      digits += s[k];
  }
  // With an exponent below 0.0001 and from 1e16 on, plain otherwise.
  if (exponent < -4 || exponent >= 16) {
    std::string out = sign + digits.substr(0, 1);
    if (digits.size() > 1)
      out += "." + digits.substr(1);
    char tail[8];
    std::snprintf(tail, sizeof tail, "e%c%02d", exponent < 0 ? '-' : '+', std::abs(exponent));
    return out + tail;
  }
  if (exponent < 0)
    return sign + "0." + std::string(static_cast<size_t>(-exponent - 1), '0') + digits;
  size_t whole = static_cast<size_t>(exponent) + 1;
  if (digits.size() <= whole)
    return sign + digits + std::string(whole - digits.size(), '0') + ".0";
  return sign + digits.substr(0, whole) + "." + digits.substr(whole);
}

std::string decimalText(Decimal v)
{
  // The unset quantity is the largest one the official client has.
  if (v.isUnset())
    return "170141183460469231731687303715884105727";
  return DecimalFunctions::decimalStringToDisplay(v);
}

bool number(const std::string& text, double& out)
{
  size_t first = text.find_first_not_of(" \t\r\n");
  if (first == std::string::npos)
    return false;
  size_t last = text.find_last_not_of(" \t\r\n");
  std::string trimmed = text.substr(first, last - first + 1);
  char* end = nullptr;
  out = std::strtod(trimmed.c_str(), &end);
  return end == trimmed.c_str() + trimmed.size();
}

int exerciseCode(OptionExerciseType type)
{
  switch (type) {
    case OptionExerciseType::Exercise:
      return 1;
    case OptionExerciseType::Lapse:
      return 2;
    case OptionExerciseType::DoNothing:
      return 3;
    case OptionExerciseType::Assigned:
      return 100;
    case OptionExerciseType::AutoexerciseClearing:
      return 101;
    case OptionExerciseType::Expired:
      return 102;
    case OptionExerciseType::Netting:
      return 103;
    case OptionExerciseType::AutoexerciseTrading:
      return 200;
    case OptionExerciseType::None:
      break;
  }
  return -1;
}

const char* fundPolicyCode(FundDistributionPolicyIndicator policy)
{
  switch (policy) {
    case FundDistributionPolicyIndicator::AccumulationFund:
      return "N";
    case FundDistributionPolicyIndicator::IncomeFund:
      return "Y";
    case FundDistributionPolicyIndicator::None:
      break;
  }
  return "";
}

const char* fundAssetCode(FundAssetType type)
{
  static const char* const codes[] = {"", "000", "001", "002", "003", "004", "005", "006", "007"};
  int k = static_cast<int>(type);
  return k >= 0 && k < 9 ? codes[k] : "";
}

namespace {

// A protobuf callback, counted.
#define IBX_GIVE(s, callback, m) (++ibx::protoMessagesBuilt, (s).w->callback(m), (s).given())

void tagMap(const TagValueListSPtr& tags, google::protobuf::Map<std::string, std::string>& map)
{
  if (!tags)
    return;
  for (const TagValueSPtr& tag : *tags) {
    if (tag)
      map[tag->tag] = tag->value;
  }
}

bool anyTag(const TagValueListSPtr& tags) { return tags && !tags->empty(); }

// ── Messages made of several objects ──

// A contract with its legs; a leg takes its price from the order.
void contractFields(const Contract& c, protobuf::Contract& m, const Order* order = nullptr)
{
  rows(c, m);
  if (c.comboLegs) {
    size_t at = 0;
    for (const ComboLegSPtr& leg : *c.comboLegs) {
      protobuf::ComboLeg& f = *m.add_combolegs();
      rows(leg ? *leg : ComboLeg(), f);
      if (order && order->orderComboLegs && at < order->orderComboLegs->size()) {
        const OrderComboLegSPtr& price = (*order->orderComboLegs)[at];
        if (price && price->price != UNSET_DOUBLE)
          f.set_perlegprice(price->price);
      }
      ++at;
    }
  }
  if (c.deltaNeutralContract)
    rows(*c.deltaNeutralContract, *m.mutable_deltaneutralcontract());
}

// A condition: its kind, how it is joined to the next one, then the
// fields of its kind.
void conditionFields(OrderCondition& c, protobuf::OrderCondition& m)
{
  m.set_type(static_cast<int>(c.type()));
  m.set_isconjunctionconnection(c.conjunctionConnection());
  if (OperatorCondition* op = dynamic_cast<OperatorCondition*>(&c))
    m.set_ismore(op->isMore());
  if (ContractCondition* cc = dynamic_cast<ContractCondition*>(&c)) {
    if (cc->conId() != UNSET_INTEGER)
      m.set_conid(cc->conId());
    if (!cc->exchange().empty())
      m.set_exchange(cc->exchange());
  }
  if (PriceCondition* pc = dynamic_cast<PriceCondition*>(&c)) {
    if (pc->price() != UNSET_DOUBLE)
      m.set_price(pc->price());
    if (pc->triggerMethod() != UNSET_INTEGER)
      m.set_triggermethod(pc->triggerMethod());
  } else if (TimeCondition* tc = dynamic_cast<TimeCondition*>(&c)) {
    if (!tc->time().empty())
      m.set_time(tc->time());
  } else if (MarginCondition* mc = dynamic_cast<MarginCondition*>(&c)) {
    if (mc->percent() != UNSET_INTEGER)
      m.set_percent(mc->percent());
  } else if (ExecutionCondition* ec = dynamic_cast<ExecutionCondition*>(&c)) {
    if (!ec->exchange().empty())
      m.set_exchange(ec->exchange());
    if (!ec->symbol().empty())
      m.set_symbol(ec->symbol());
    if (!ec->secType().empty())
      m.set_sectype(ec->secType());
  } else if (VolumeCondition* vc = dynamic_cast<VolumeCondition*>(&c)) {
    if (vc->volume() != UNSET_INTEGER)
      m.set_volume(vc->volume());
  } else if (PercentChangeCondition* pcc = dynamic_cast<PercentChangeCondition*>(&c)) {
    if (pcc->changePercent() != UNSET_DOUBLE)
      m.set_changepercent(pcc->changePercent());
  }
}

void tierFields(const SoftDollarTier& t, protobuf::SoftDollarTier& m)
{
  if (!t.name().empty())
    m.set_name(t.name());
  if (!t.val().empty())
    m.set_value(t.val());
  if (!t.displayName().empty())
    m.set_displayname(t.displayName());
}

void orderFields(const Order& o, protobuf::Order& m)
{
  rows(o, m);
  if (anyTag(o.smartComboRoutingParams))
    tagMap(o.smartComboRoutingParams, *m.mutable_smartcomboroutingparams());
  if (!o.hedgeType.empty() && !o.hedgeParam.empty())
    m.set_hedgeparam(o.hedgeParam);
  // The parameters of an algo come with its name only.
  if (!o.algoStrategy.empty()) {
    m.set_algostrategy(o.algoStrategy);
    if (anyTag(o.algoParams))
      tagMap(o.algoParams, *m.mutable_algoparams());
  }
  for (const std::shared_ptr<OrderCondition>& c : o.conditions) {
    if (c)
      conditionFields(*c, *m.add_conditions());
  }
  const SoftDollarTier& tier = o.softDollarTier;
  if (!tier.name().empty() || !tier.val().empty() || !tier.displayName().empty())
    tierFields(tier, *m.mutable_softdollartier());
}

void orderStateFields(const OrderState& s, protobuf::OrderState& m)
{
  rows(s, m);
  if (s.orderAllocations) {
    for (const OrderAllocationSPtr& a : *s.orderAllocations)
      rows(a ? *a : OrderAllocation(), *m.add_orderallocations());
  }
}

// The two messages of a contract's details: its contract and its details.
void detailsFields(const ContractDetails& d, bool bond, protobuf::ContractData& m)
{
  protobuf::Contract& contract = *m.mutable_contract();
  contractFields(d.contract, contract);
  // The official client splits the last trade date of the contract into a
  // date, a time and a time zone: they are put back together.
  std::string parts[3] = {bond ? d.maturity : d.contract.lastTradeDateOrContractMonth, d.lastTradeTime,
                          d.lastTradeTime.empty() ? std::string() : d.timeZoneId};
  int count = 3;
  while (count > 0 && parts[count - 1].empty())
    --count;
  if (count > 0 && !parts[0].empty()) {
    std::string joined = parts[0];
    for (int k = 1; k < count; ++k)
      joined += " " + parts[k];
    contract.set_lasttradedateorcontractmonth(joined);
  }
  protobuf::ContractDetails& details = *m.mutable_contractdetails();
  rows(d, details);
  if (anyTag(d.secIdList))
    tagMap(d.secIdList, *details.mutable_secidlist());
  if (d.ineligibilityReasonList) {
    for (const IneligibilityReasonSPtr& reason : *d.ineligibilityReasonList)
      rows(reason ? *reason : IneligibilityReason(), *details.add_ineligibilityreasonlist());
  }
}

// The attributes of a tick: each is set, yes or no.
void lastFlags(const TickAttribLast& a, protobuf::TickAttribLast& m)
{
  m.set_pastlimit(a.pastLimit);
  m.set_unreported(a.unreported);
}

void bidAskFlags(const TickAttribBidAsk& a, protobuf::TickAttribBidAsk& m)
{
  m.set_bidpastlow(a.bidPastLow);
  m.set_askpasthigh(a.askPastHigh);
}

// The size tick a price comes with in one answer.
TickType sizeTickOf(TickType price)
{
  switch (price) {
    case BID:
      return BID_SIZE;
    case ASK:
      return ASK_SIZE;
    case LAST:
      return LAST_SIZE;
    case DELAYED_BID:
      return DELAYED_BID_SIZE;
    case DELAYED_ASK:
      return DELAYED_ASK_SIZE;
    case DELAYED_LAST:
      return DELAYED_LAST_SIZE;
    default:
      return NOT_SET;
  }
}

// The price that waited: its message, with the size when it came, then
// its plain callback.
void givePrice(Sink& s, const Decimal* size)
{
  ProtoState& p = s.proto;
  s.hold(ProtoState::NOTHING);
  try {
    protobuf::TickPrice m;
    head_tickPrice(m, p.reqId, p.field, p.price, p.attrib);
    if (size)
      m.set_size(decimalText(*size));
    m.set_attrmask((p.attrib.canAutoExecute ? 1 : 0) | (p.attrib.pastLimit ? 2 : 0) | (p.attrib.preOpen ? 4 : 0));
    IBX_GIVE(s, tickPriceProtoBuf, m);
    s.w->tickPrice(p.reqId, p.field, p.price, p.attrib);
  } catch (...) {
    s.failed();
  }
}

// The bars that waited: one message for all, then the callback of each.
void giveBars(Sink& s)
{
  ProtoState& p = s.proto;
  s.hold(ProtoState::NOTHING);
  std::vector<Bar> bars;
  bars.swap(p.bars);
  try {
    protobuf::HistoricalData m;
    m.set_reqid(p.reqId);
    for (const Bar& bar : bars)
      rows(bar, *m.add_historicaldatabars());
    IBX_GIVE(s, historicalDataProtoBuf, m);
  } catch (...) {
    s.failed();
  }
  for (const Bar& bar : bars) {
    try {
      s.w->historicalData(p.reqId, bar);
    } catch (...) {
      s.failed();
    }
  }
}

// The rows of a scan that waited: one message for all, then the callback
// of each.
void giveRows(Sink& s)
{
  ProtoState& p = s.proto;
  s.hold(ProtoState::NOTHING);
  std::vector<ProtoState::Row> scan;
  scan.swap(p.rows);
  try {
    protobuf::ScannerData m;
    m.set_reqid(p.reqId);
    for (const ProtoState::Row& row : scan) {
      protobuf::ScannerDataElement& e = *m.add_scannerdataelement();
      if (row.rank != UNSET_INTEGER)
        e.set_rank(row.rank);
      if (!row.details.marketName.empty())
        e.set_marketname(row.details.marketName);
      if (!row.distance.empty())
        e.set_distance(row.distance);
      if (!row.benchmark.empty())
        e.set_benchmark(row.benchmark);
      if (!row.projection.empty())
        e.set_projection(row.projection);
      if (!row.legsStr.empty())
        e.set_combokey(row.legsStr);
      contractFields(row.details.contract, *e.mutable_contract());
    }
    IBX_GIVE(s, scannerDataProtoBuf, m);
  } catch (...) {
    s.failed();
  }
  for (const ProtoState::Row& row : scan) {
    try {
      s.w->scannerData(p.reqId, row.rank, row.details, row.distance, row.benchmark, row.projection, row.legsStr);
    } catch (...) {
      s.failed();
    }
  }
}

}  // namespace

// ── What waits for the next callback ──

void flush(Sink& s)
{
  switch (s.proto.held) {
    case ProtoState::PRICE:
      givePrice(s, nullptr);
      break;
    case ProtoState::BARS:
      giveBars(s);
      break;
    case ProtoState::ROWS:
      giveRows(s);
      break;
    case ProtoState::NOTHING:
      break;
  }
}

void bind(Sink& s)
{
  s.proto.source = s.w->ibxUnusedProtoCallbacks();
  s.given();
}

// ── Callbacks with a rule of their own ──

// A price waits for the next callback: when that one is its size, the
// message of the price carries it and the size has no message of its own.
bool tickPrice(Sink& s, int reqId, TickType field, double price, const TickAttrib& attrib)
{
  s.flush();
  if (!s.wants(PB_tickPrice))
    return true;
  ProtoState& p = s.proto;
  s.hold(ProtoState::PRICE);
  p.reqId = reqId;
  p.field = field;
  p.price = price;
  p.attrib = attrib;
  return false;
}

bool tickSize(Sink& s, int reqId, TickType field, Decimal size)
{
  ProtoState& p = s.proto;
  if (p.held == ProtoState::PRICE && p.reqId == reqId && field != NOT_SET && sizeTickOf(p.field) == field) {
    givePrice(s, &size);
    return true;
  }
  s.flush();
  if (s.wants(PB_tickSize)) {
    protobuf::TickSize m;
    head_tickSize(m, reqId, field, size);
    IBX_GIVE(s, tickSizeProtoBuf, m);
  }
  return true;
}

// The message carries the last price and last size precisions of the
// contract, which the plain callback does not have.
bool tickReqParams(Sink& s, int reqId, double minTick, const std::string& bboExchange, int snapshotPermissions)
{
  s.flush();
  if (s.wants(PB_tickReqParams)) {
    protobuf::TickReqParams m;
    head_tickReqParams(m, reqId, minTick, bboExchange, snapshotPermissions);
    char price[64], size[64];
    size_t priceLength = 0, sizeLength = 0;
    if (s.proto.session && ibx_precisions(s.proto.session, reqId, price, sizeof price, &priceLength, size, sizeof size, &sizeLength)) {
      if (priceLength)
        m.set_lastpriceprecision(std::string(price, std::min(priceLength, sizeof price)));
      if (sizeLength)
        m.set_lastsizeprecision(std::string(size, std::min(sizeLength, sizeof size)));
    }
    IBX_GIVE(s, tickReqParamsProtoBuf, m);
  }
  return true;
}

// The bars of an answer come in one message before the callback of each:
// they wait until another callback comes.
bool historicalData(Sink& s, int reqId, const Bar& bar)
{
  ProtoState& p = s.proto;
  if (p.held == ProtoState::BARS && p.reqId == reqId) {
    p.bars.push_back(bar);
    return false;
  }
  s.flush();
  if (!s.wants(PB_historicalData))
    return true;
  s.hold(ProtoState::BARS);
  p.reqId = reqId;
  p.bars.push_back(bar);
  return false;
}

// The same for the rows of a scan.
bool scannerData(Sink& s, int reqId, int rank, const ContractDetails& contractDetails, const std::string& distance,
                 const std::string& benchmark, const std::string& projection, const std::string& legsStr)
{
  ProtoState& p = s.proto;
  if (!(p.held == ProtoState::ROWS && p.reqId == reqId)) {
    s.flush();
    if (!s.wants(PB_scannerData))
      return true;
    s.hold(ProtoState::ROWS);
    p.reqId = reqId;
  }
  p.rows.push_back(ProtoState::Row{rank, contractDetails, distance, benchmark, projection, legsStr});
  return false;
}

// The end of a scan has no message of its own. A scan with no row still
// has its message, with no row.
bool scannerDataEnd(Sink& s, int reqId)
{
  ProtoState& p = s.proto;
  bool given = p.held == ProtoState::ROWS && p.reqId == reqId;
  s.flush();
  if (!given && s.wants(PB_scannerData)) {
    try {
      protobuf::ScannerData m;
      m.set_reqid(reqId);
      IBX_GIVE(s, scannerDataProtoBuf, m);
    } catch (...) {
      s.failed();
    }
  }
  return true;
}

// An error of the servers has its message; one the official client makes
// itself has none.
bool error(Sink& s, int id, long long errorTimeMs, int errorCode, const std::string& errorString, const std::string& advancedOrderRejectJson)
{
  s.flush();
  if (!clientError(errorCode) && s.wants(PB_error)) {
    protobuf::ErrorMessage m;
    m.set_id(id);
    m.set_errortime(errorTimeMs);
    m.set_errorcode(errorCode);
    m.set_errormsg(errorString);
    if (!advancedOrderRejectJson.empty())
      m.set_advancedorderrejectjson(advancedOrderRejectJson);
    IBX_GIVE(s, errorProtoBuf, m);
  }
  return true;
}

// ── Messages with a shape of their own ──

void openOrder(protobuf::OpenOrder& m, int orderId, const Contract& contract, const Order& order, const OrderState& orderState)
{
  m.set_orderid(orderId);
  contractFields(contract, *m.mutable_contract(), &order);
  orderFields(order, *m.mutable_order());
  orderStateFields(orderState, *m.mutable_orderstate());
}

void completedOrder(protobuf::CompletedOrder& m, const Contract& contract, const Order& order, const OrderState& orderState)
{
  contractFields(contract, *m.mutable_contract(), &order);
  orderFields(order, *m.mutable_order());
  orderStateFields(orderState, *m.mutable_orderstate());
}

void updatePortfolio(protobuf::PortfolioValue& m, const Contract& contract, Decimal position, double marketPrice, double marketValue,
                     double averageCost, double unrealizedPNL, double realizedPNL, const std::string& accountName)
{
  head_updatePortfolio(m, contract, position, marketPrice, marketValue, averageCost, unrealizedPNL, realizedPNL, accountName);
  contractFields(contract, *m.mutable_contract());
}

void position(protobuf::Position& m, const std::string& account, const Contract& contract, Decimal position, double avgCost)
{
  head_position(m, account, contract, position, avgCost);
  contractFields(contract, *m.mutable_contract());
}

void positionMulti(protobuf::PositionMulti& m, int reqId, const std::string& account, const std::string& modelCode, const Contract& contract,
                   Decimal pos, double avgCost)
{
  head_positionMulti(m, reqId, account, modelCode, contract, pos, avgCost);
  contractFields(contract, *m.mutable_contract());
}

void contractDetails(protobuf::ContractData& m, int reqId, const ContractDetails& contractDetails)
{
  m.set_reqid(reqId);
  detailsFields(contractDetails, false, m);
}

void bondContractDetails(protobuf::ContractData& m, int reqId, const ContractDetails& contractDetails)
{
  m.set_reqid(reqId);
  detailsFields(contractDetails, true, m);
}

void execDetails(protobuf::ExecutionDetails& m, int reqId, const Contract& contract, const Execution& execution)
{
  m.set_reqid(reqId);
  contractFields(contract, *m.mutable_contract());
  rows(execution, *m.mutable_execution());
}

// A field of the report is left out when it has the value a report starts with.
void commissionAndFeesReport(protobuf::CommissionAndFeesReport& m, const CommissionAndFeesReport& r)
{
  if (!r.execId.empty())
    m.set_execid(r.execId);
  if (r.commissionAndFees != UNSET_DOUBLE && r.commissionAndFees != 0)
    m.set_commissionandfees(r.commissionAndFees);
  if (!r.currency.empty())
    m.set_currency(r.currency);
  if (r.realizedPNL != UNSET_DOUBLE && r.realizedPNL != 0)
    m.set_realizedpnl(r.realizedPNL);
  if (r.yield != UNSET_DOUBLE && r.yield != 0)
    m.set_bondyield(r.yield);
  if (r.yieldRedemptionDate != 0)
    m.set_yieldredemptiondate(std::to_string(r.yieldRedemptionDate));
}

void historicalDataUpdate(protobuf::HistoricalDataUpdate& m, int reqId, const Bar& bar)
{
  m.set_reqid(reqId);
  rows(bar, *m.mutable_historicaldatabar());
}

void securityDefinitionOptionalParameter(protobuf::SecDefOptParameter& m, int reqId, const std::string& exchange, int underlyingConId,
                                         const std::string& tradingClass, const std::string& multiplier,
                                         const std::set<std::string>& expirations, const std::set<double>& strikes)
{
  head_securityDefinitionOptionalParameter(m, reqId, exchange, underlyingConId, tradingClass, multiplier, expirations, strikes);
  for (const std::string& expiration : expirations)
    m.add_expirations(expiration);
  for (double strike : strikes)
    m.add_strikes(strike);
}

void softDollarTiers(protobuf::SoftDollarTiers& m, int reqId, const std::vector<SoftDollarTier>& tiers)
{
  m.set_reqid(reqId);
  for (const SoftDollarTier& tier : tiers)
    tierFields(tier, *m.add_softdollartiers());
}

void familyCodes(protobuf::FamilyCodes& m, const std::vector<FamilyCode>& familyCodes)
{
  for (const FamilyCode& code : familyCodes) {
    protobuf::FamilyCode& f = *m.add_familycodes();
    if (!code.accountID.empty())
      f.set_accountid(code.accountID);
    if (!code.familyCodeStr.empty())
      f.set_familycode(code.familyCodeStr);
  }
}

void symbolSamples(protobuf::SymbolSamples& m, int reqId, const std::vector<ContractDescription>& contractDescriptions)
{
  m.set_reqid(reqId);
  for (const ContractDescription& d : contractDescriptions) {
    protobuf::ContractDescription& f = *m.add_contractdescriptions();
    contractFields(d.contract, *f.mutable_contract());
    for (const std::string& type : d.derivativeSecTypes)
      f.add_derivativesectypes(type);
  }
}

void smartComponents(protobuf::SmartComponents& m, int reqId, const SmartComponentsMap& theMap)
{
  m.set_reqid(reqId);
  for (const SmartComponentsMap::value_type& item : theMap) {
    protobuf::SmartComponent& f = *m.add_smartcomponents();
    if (!std::get<0>(item.second).empty())
      f.set_exchange(std::get<0>(item.second));
    if (std::get<1>(item.second) != '\0')
      f.set_exchangeletter(std::string(1, std::get<1>(item.second)));
    // The reference leaves the bit 0 out.
    if (item.first != 0)
      f.set_bitnumber(item.first);
  }
}

void mktDepthExchanges(protobuf::MarketDepthExchanges& m, const std::vector<DepthMktDataDescription>& depthMktDataDescriptions)
{
  for (const DepthMktDataDescription& d : depthMktDataDescriptions)
    rows(d, *m.add_depthmarketdatadescriptions());
}

void newsProviders(protobuf::NewsProviders& m, const std::vector<NewsProvider>& newsProviders)
{
  for (const NewsProvider& provider : newsProviders) {
    protobuf::NewsProvider& f = *m.add_newsproviders();
    if (!provider.providerCode.empty())
      f.set_providercode(provider.providerCode);
    if (!provider.providerName.empty())
      f.set_providername(provider.providerName);
  }
}

void histogramData(protobuf::HistogramData& m, int reqId, const HistogramDataVector& data)
{
  m.set_reqid(reqId);
  for (const HistogramEntry& entry : data) {
    protobuf::HistogramDataEntry& f = *m.add_histogramdataentries();
    if (entry.price != UNSET_DOUBLE)
      f.set_price(entry.price);
    if (!entry.size.isUnset())
      f.set_size(decimalText(entry.size));
  }
}

void marketRule(protobuf::MarketRule& m, int marketRuleId, const std::vector<PriceIncrement>& priceIncrements)
{
  m.set_marketruleid(marketRuleId);
  for (const PriceIncrement& increment : priceIncrements) {
    protobuf::PriceIncrement& f = *m.add_priceincrements();
    if (increment.lowEdge != UNSET_DOUBLE)
      f.set_lowedge(increment.lowEdge);
    if (increment.increment != UNSET_DOUBLE)
      f.set_increment(increment.increment);
  }
}

void historicalTicks(protobuf::HistoricalTicks& m, int reqId, const std::vector<HistoricalTick>& ticks, bool done)
{
  m.set_reqid(reqId);
  for (const HistoricalTick& tick : ticks)
    rows(tick, *m.add_historicalticks());
  if (done)
    m.set_isdone(true);
}

void historicalTicksBidAsk(protobuf::HistoricalTicksBidAsk& m, int reqId, const std::vector<HistoricalTickBidAsk>& ticks, bool done)
{
  m.set_reqid(reqId);
  for (const HistoricalTickBidAsk& tick : ticks) {
    protobuf::HistoricalTickBidAsk& f = *m.add_historicalticksbidask();
    rows(tick, f);
    bidAskFlags(tick.tickAttribBidAsk, *f.mutable_tickattribbidask());
  }
  if (done)
    m.set_isdone(true);
}

void historicalTicksLast(protobuf::HistoricalTicksLast& m, int reqId, const std::vector<HistoricalTickLast>& ticks, bool done)
{
  m.set_reqid(reqId);
  for (const HistoricalTickLast& tick : ticks) {
    protobuf::HistoricalTickLast& f = *m.add_historicaltickslast();
    rows(tick, f);
    lastFlags(tick.tickAttribLast, *f.mutable_tickattriblast());
  }
  if (done)
    m.set_isdone(true);
}

void tickByTickAllLast(protobuf::TickByTickData& m, int reqId, int tickType, long long time, double price, Decimal size,
                       const TickAttribLast& tickAttribLast, const std::string& exchange, const std::string& specialConditions)
{
  m.set_reqid(reqId);
  m.set_ticktype(tickType);
  protobuf::HistoricalTickLast& f = *m.mutable_historicalticklast();
  if (time != UNSET_INTEGER && time != UNSET_LLONG)
    f.set_time(time);
  if (price != UNSET_DOUBLE)
    f.set_price(price);
  if (!size.isUnset())
    f.set_size(decimalText(size));
  if (!exchange.empty())
    f.set_exchange(exchange);
  if (!specialConditions.empty())
    f.set_specialconditions(specialConditions);
  lastFlags(tickAttribLast, *f.mutable_tickattriblast());
}

void tickByTickBidAsk(protobuf::TickByTickData& m, int reqId, long long time, double bidPrice, double askPrice, Decimal bidSize,
                      Decimal askSize, const TickAttribBidAsk& tickAttribBidAsk)
{
  m.set_reqid(reqId);
  m.set_ticktype(3);
  protobuf::HistoricalTickBidAsk& f = *m.mutable_historicaltickbidask();
  if (time != UNSET_INTEGER && time != UNSET_LLONG)
    f.set_time(time);
  if (bidPrice != UNSET_DOUBLE)
    f.set_pricebid(bidPrice);
  if (askPrice != UNSET_DOUBLE)
    f.set_priceask(askPrice);
  if (!bidSize.isUnset())
    f.set_sizebid(decimalText(bidSize));
  if (!askSize.isUnset())
    f.set_sizeask(decimalText(askSize));
  bidAskFlags(tickAttribBidAsk, *f.mutable_tickattribbidask());
}

void tickByTickMidPoint(protobuf::TickByTickData& m, int reqId, long long time, double midPoint)
{
  m.set_reqid(reqId);
  m.set_ticktype(4);
  protobuf::HistoricalTick& f = *m.mutable_historicaltickmidpoint();
  if (time != UNSET_INTEGER && time != UNSET_LLONG)
    f.set_time(time);
  if (midPoint != UNSET_DOUBLE)
    f.set_price(midPoint);
}

namespace {

void depthFields(protobuf::MarketDepthData& f, int position, int operation, int side, double price, Decimal size)
{
  if (position != UNSET_INTEGER)
    f.set_position(position);
  if (operation != UNSET_INTEGER)
    f.set_operation(operation);
  if (side != UNSET_INTEGER)
    f.set_side(side);
  if (price != UNSET_DOUBLE)
    f.set_price(price);
  if (!size.isUnset())
    f.set_size(decimalText(size));
}

}  // namespace

void updateMktDepth(protobuf::MarketDepth& m, int reqId, int position, int operation, int side, double price, Decimal size)
{
  m.set_reqid(reqId);
  depthFields(*m.mutable_marketdepthdata(), position, operation, side, price, size);
}

void updateMktDepthL2(protobuf::MarketDepthL2& m, int reqId, int position, const std::string& marketMaker, int operation, int side,
                      double price, Decimal size, bool isSmartDepth)
{
  m.set_reqid(reqId);
  protobuf::MarketDepthData& f = *m.mutable_marketdepthdata();
  depthFields(f, position, operation, side, price, size);
  if (!marketMaker.empty())
    f.set_marketmaker(marketMaker);
  if (isSmartDepth)
    f.set_issmartdepth(true);
}

void historicalSchedule(protobuf::HistoricalSchedule& m, int reqId, const std::string& startDateTime, const std::string& endDateTime,
                        const std::string& timeZone, const std::vector<HistoricalSession>& sessions)
{
  head_historicalSchedule(m, reqId, startDateTime, endDateTime, timeZone, sessions);
  for (const HistoricalSession& session : sessions) {
    protobuf::HistoricalSession& f = *m.add_historicalsessions();
    if (!session.startDateTime.empty())
      f.set_startdatetime(session.startDateTime);
    if (!session.endDateTime.empty())
      f.set_enddatetime(session.endDateTime);
    if (!session.refDate.empty())
      f.set_refdate(session.refDate);
  }
}

// ── Request messages as the objects of the plain requests ──

std::string floatMaxText(double v)
{
  if (v == UNSET_DOUBLE)
    return std::string();
  char text[400];
  std::snprintf(text, sizeof text, "%.8f", v);
  std::string s(text);
  s.erase(s.find_last_not_of('0') + 1);
  if (!s.empty() && s.back() == '.')
    s.pop_back();
  return s;
}

TagValueListSPtr tagsOf(const google::protobuf::Map<std::string, std::string>& map)
{
  if (map.empty())
    return TagValueListSPtr();
  // A map has no order of its own: the names in theirs.
  std::vector<std::pair<std::string, std::string>> items(map.begin(), map.end());
  std::sort(items.begin(), items.end());
  TagValueListSPtr tags = std::make_shared<TagValueList>();
  for (const std::pair<std::string, std::string>& item : items)
    tags->push_back(std::make_shared<TagValue>(item.first, item.second));
  return tags;
}

Contract contractOf(const protobuf::Contract& p, Order* order)
{
  Contract c;
  copy(p, c);
  if (p.combolegs_size() > 0) {
    c.comboLegs = std::make_shared<Contract::ComboLegList>();
    Order::OrderComboLegListSPtr prices = std::make_shared<Order::OrderComboLegList>();
    bool priced = false;
    for (const protobuf::ComboLeg& leg : p.combolegs()) {
      ComboLegSPtr made = std::make_shared<ComboLeg>();
      copy(leg, *made);
      c.comboLegs->push_back(made);
      OrderComboLegSPtr price = std::make_shared<OrderComboLeg>();
      if (leg.has_perlegprice()) {
        price->price = leg.perlegprice();
        priced = true;
      }
      prices->push_back(price);
    }
    // The prices of the legs are the order's, one for each leg.
    if (order && priced)
      order->orderComboLegs = prices;
  }
  if (p.has_deltaneutralcontract()) {
    const protobuf::DeltaNeutralContract& d = p.deltaneutralcontract();
    c.deltaNeutralContract.reset(new DeltaNeutralContract());
    c.deltaNeutralContract->conId = d.has_conid() ? d.conid() : UNSET_INTEGER;
    c.deltaNeutralContract->delta = d.has_delta() ? d.delta() : UNSET_DOUBLE;
    c.deltaNeutralContract->price = d.has_price() ? d.price() : UNSET_DOUBLE;
  }
  return c;
}

namespace {

// A condition of a request; null for a kind the official client does not have.
std::shared_ptr<OrderCondition> conditionOf(const protobuf::OrderCondition& p)
{
  int kind = p.has_type() ? p.type() : UNSET_INTEGER;
  switch (kind) {
    case OrderCondition::Price:
    case OrderCondition::Time:
    case OrderCondition::Margin:
    case OrderCondition::Execution:
    case OrderCondition::Volume:
    case OrderCondition::PercentChange:
      break;
    default:
      return nullptr;
  }
  std::shared_ptr<OrderCondition> c(OrderCondition::create(static_cast<OrderCondition::OrderConditionType>(kind)));
  // Joined to the next one by "and" unless the message says otherwise.
  c->conjunctionConnection(p.has_isconjunctionconnection() ? p.isconjunctionconnection() : true);
  if (OperatorCondition* op = dynamic_cast<OperatorCondition*>(c.get()))
    op->isMore(p.has_ismore() ? p.ismore() : false);
  if (ContractCondition* cc = dynamic_cast<ContractCondition*>(c.get())) {
    if (p.has_conid())
      cc->conId(p.conid());
    if (p.has_exchange())
      cc->exchange(p.exchange());
  }
  if (PriceCondition* pc = dynamic_cast<PriceCondition*>(c.get())) {
    if (p.has_price())
      pc->price(p.price());
    if (p.has_triggermethod())
      pc->triggerMethod(static_cast<int>(p.triggermethod()));
  } else if (TimeCondition* tc = dynamic_cast<TimeCondition*>(c.get())) {
    if (p.has_time())
      tc->time(p.time());
  } else if (MarginCondition* mc = dynamic_cast<MarginCondition*>(c.get())) {
    if (p.has_percent())
      mc->percent(p.percent());
  } else if (ExecutionCondition* ec = dynamic_cast<ExecutionCondition*>(c.get())) {
    if (p.has_sectype())
      ec->secType(p.sectype());
    if (p.has_exchange())
      ec->exchange(p.exchange());
    if (p.has_symbol())
      ec->symbol(p.symbol());
  } else if (VolumeCondition* vc = dynamic_cast<VolumeCondition*>(c.get())) {
    if (p.has_volume())
      vc->volume(p.volume());
  } else if (PercentChangeCondition* pcc = dynamic_cast<PercentChangeCondition*>(c.get())) {
    if (p.has_changepercent())
      pcc->changePercent(p.changepercent());
  }
  return c;
}

OrderCancel orderCancelOf(bool given, const protobuf::OrderCancel& p)
{
  OrderCancel cancel;
  if (given)
    copy(p, cancel);
  return cancel;
}

}  // namespace

bool orderOf(const protobuf::Order& p, const protobuf::AttachedOrders* attached, Order& o, std::string& refused)
{
  copy(p, o);
  // An order that includes the night in its conditions is refused by the
  // plain request: the message says so too.
  if (p.has_conditionsincludeovernight())
    o.conditionsIncludeOvernight = p.conditionsincludeovernight();
  if (TagValueListSPtr tags = tagsOf(p.algoparams()))
    o.algoParams = tags;
  if (TagValueListSPtr tags = tagsOf(p.smartcomboroutingparams()))
    o.smartComboRoutingParams = tags;
  if (TagValueListSPtr tags = tagsOf(p.ordermiscoptions()))
    o.orderMiscOptions = tags;
  for (const protobuf::OrderCondition& condition : p.conditions()) {
    std::shared_ptr<OrderCondition> made = conditionOf(condition);
    if (!made) {
      refused = "an order condition of type " + (condition.has_type() ? std::to_string(condition.type()) : std::string("none"))
              + " is not one of the official client";
      return false;
    }
    o.conditions.push_back(made);
  }
  if (p.has_softdollartier()) {
    const protobuf::SoftDollarTier& t = p.softdollartier();
    o.softDollarTier = SoftDollarTier(t.has_name() ? t.name() : std::string(), t.has_value() ? t.value() : std::string(),
                                      t.has_displayname() ? t.displayname() : std::string());
  }
  if (attached)
    copy(*attached, o);
  return true;
}

// ── The configuration: a message as text records ──
//
// The configuration crosses the C entry layer as one record for each
// field that is set, `path \x1f value \x1e`: the path is made of field
// names, with an index after the name of a repeated field, whose count is
// the record `path.#`; a sub-message that is set has a record of its own
// with an empty value. The messages are walked by their descriptors: a
// field is found by its name without case and underscores, which is the
// name the other side has for it.

namespace {

typedef google::protobuf::Message Message;
typedef google::protobuf::FieldDescriptor Field;

std::string plainName(const std::string& name)
{
  std::string out;
  for (char c : name) {
    if (c != '_')
      out += static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
  }
  return out;
}

void record(std::string& out, const std::string& path, const std::string& value)
{
  out += path;
  out += '\x1f';
  out += value;
  out += '\x1e';
}

// The value of a field as a text; `index` is -1 for a field that is not repeated.
std::string valueText(const Message& m, const Field* f, int index)
{
  const google::protobuf::Reflection* r = m.GetReflection();
  switch (f->cpp_type()) {
    case Field::CPPTYPE_INT32:
      return std::to_string(index < 0 ? r->GetInt32(m, f) : r->GetRepeatedInt32(m, f, index));
    case Field::CPPTYPE_INT64:
      return std::to_string(index < 0 ? r->GetInt64(m, f) : r->GetRepeatedInt64(m, f, index));
    case Field::CPPTYPE_BOOL:
      return (index < 0 ? r->GetBool(m, f) : r->GetRepeatedBool(m, f, index)) ? "true" : "false";
    case Field::CPPTYPE_DOUBLE:
      return floatText(index < 0 ? r->GetDouble(m, f) : r->GetRepeatedDouble(m, f, index));
    case Field::CPPTYPE_STRING:
      return index < 0 ? r->GetString(m, f) : r->GetRepeatedString(m, f, index);
    default:
      return std::string();
  }
}

void writeRecords(const Message& m, const std::string& path, std::string& out)
{
  const google::protobuf::Reflection* r = m.GetReflection();
  std::vector<const Field*> set;
  r->ListFields(m, &set);
  for (const Field* f : set) {
    std::string at = (path.empty() ? std::string() : path + ".") + std::string(f->name());
    bool message = f->cpp_type() == Field::CPPTYPE_MESSAGE;
    if (f->is_repeated()) {
      int count = r->FieldSize(m, f);
      record(out, at + ".#", std::to_string(count));
      for (int k = 0; k < count; ++k) {
        std::string item = at + "." + std::to_string(k);
        record(out, item, message ? std::string() : valueText(m, f, k));
        if (message)
          writeRecords(r->GetRepeatedMessage(m, f, k), item, out);
      }
    } else if (message) {
      record(out, at, "");
      writeRecords(r->GetMessage(m, f), at, out);
    } else {
      record(out, at, valueText(m, f, -1));
    }
  }
}

const Field* fieldOf(const Message& m, const std::string& name)
{
  const google::protobuf::Descriptor* d = m.GetDescriptor();
  std::string wanted = plainName(name);
  for (int k = 0; k < d->field_count(); ++k) {
    if (plainName(std::string(d->field(k)->name())) == wanted)
      return d->field(k);
  }
  return nullptr;
}

// Sets a field from its text, or adds it to a repeated one.
void takeValue(Message& m, const Field* f, const std::string& value, bool add)
{
  const google::protobuf::Reflection* r = m.GetReflection();
  switch (f->cpp_type()) {
    case Field::CPPTYPE_INT32: {
      int v = static_cast<int>(std::strtol(value.c_str(), nullptr, 10));
      add ? r->AddInt32(&m, f, v) : r->SetInt32(&m, f, v);
      break;
    }
    case Field::CPPTYPE_INT64: {
      long long v = std::strtoll(value.c_str(), nullptr, 10);
      add ? r->AddInt64(&m, f, v) : r->SetInt64(&m, f, v);
      break;
    }
    case Field::CPPTYPE_BOOL:
      add ? r->AddBool(&m, f, value == "true") : r->SetBool(&m, f, value == "true");
      break;
    case Field::CPPTYPE_DOUBLE: {
      double v = std::strtod(value.c_str(), nullptr);
      add ? r->AddDouble(&m, f, v) : r->SetDouble(&m, f, v);
      break;
    }
    case Field::CPPTYPE_STRING:
      add ? r->AddString(&m, f, value) : r->SetString(&m, f, value);
      break;
    default:
      break;
  }
}

// One record: the message it names is reached field by field, made on
// the way, and its last name is the field set. A name the message does
// not have is left out.
void takeRecord(Message& root, const std::string& path, const std::string& value)
{
  std::vector<std::string> names;
  for (size_t at = 0; at <= path.size();) {
    size_t dot = path.find('.', at);
    if (dot == std::string::npos)
      dot = path.size();
    names.push_back(path.substr(at, dot - at));
    at = dot + 1;
  }
  Message* m = &root;
  for (size_t k = 0; k < names.size(); ++k) {
    const Field* f = fieldOf(*m, names[k]);
    if (!f)
      return;
    const google::protobuf::Reflection* r = m->GetReflection();
    bool last = k + 1 == names.size();
    bool message = f->cpp_type() == Field::CPPTYPE_MESSAGE;
    if (f->is_repeated()) {
      if (last)
        return;
      const std::string& next = names[k + 1];
      if (!message) {
        // The items of a repeated text come in their order.
        if (next != "#")
          takeValue(*m, f, value, true);
        return;
      }
      // The count makes the items; an index names one of them.
      int wanted = next == "#" ? std::atoi(value.c_str()) : std::atoi(next.c_str()) + 1;
      while (r->FieldSize(*m, f) < wanted)
        r->AddMessage(m, f);
      if (next == "#" || k + 2 == names.size())
        return;
      m = r->MutableRepeatedMessage(m, f, std::atoi(next.c_str()));
      ++k;
    } else if (message) {
      m = r->MutableMessage(m, f);
    } else {
      if (last)
        takeValue(*m, f, value, false);
      return;
    }
  }
}

}  // namespace

std::string recordsOf(const google::protobuf::Message& m)
{
  std::string out;
  writeRecords(m, std::string(), out);
  return out;
}

void fromRecords(ibx_str records, google::protobuf::Message& m)
{
  std::string text = ibx::str(records);
  for (size_t at = 0; at < text.size();) {
    size_t end = text.find('\x1e', at);
    if (end == std::string::npos)
      end = text.size();
    size_t mid = text.find('\x1f', at);
    if (mid != std::string::npos && mid < end)
      takeRecord(m, text.substr(at, mid - at), text.substr(mid + 1, end - mid - 1));
    at = end + 1;
  }
}

// The answers of the two configuration requests have no plain callback:
// the protobuf callback is all there is.
void configResponse(Sink& s, ibx_str fields)
{
  s.flush();
  if (!s.wants(PB_configResponse))
    return;
  protobuf::ConfigResponse m;
  fromRecords(fields, m);
  IBX_GIVE(s, configResponseProtoBuf, m);
}

void updateConfigResponse(Sink& s, ibx_str fields)
{
  s.flush();
  if (!s.wants(PB_updateConfigResponse))
    return;
  protobuf::UpdateConfigResponse m;
  fromRecords(fields, m);
  IBX_GIVE(s, updateConfigResponseProtoBuf, m);
}

}  // namespace pb
}  // namespace ibx

// ── Requests whose message is not read field by field ──

void EClient::placeOrderProtoBuf(const protobuf::PlaceOrderRequest& p)
{
  int id = p.has_orderid() ? p.orderid() : 0;
  Order order;
  std::string refused;
  if (p.has_order() && !ibx::pb::orderOf(p.order(), p.has_attachedorders() ? &p.attachedorders() : nullptr, order, refused)) {
    // Nothing is sent: the order would go out without one of its conditions.
    m_pEWrapper->error(id, Utils::currentTimeMillis(), 321, "placeOrderProtoBuf: " + refused, "");
    return;
  }
  Contract contract = p.has_contract() ? ibx::pb::contractOf(p.contract(), &order) : Contract();
  placeOrder(id, contract, order);
}

void EClient::cancelOrderProtoBuf(const protobuf::CancelOrderRequest& p)
{
  cancelOrder(p.has_orderid() ? p.orderid() : 0, ibx::pb::orderCancelOf(p.has_ordercancel(), p.ordercancel()));
}

void EClient::reqGlobalCancelProtoBuf(const protobuf::GlobalCancelRequest& p)
{
  reqGlobalCancel(ibx::pb::orderCancelOf(p.has_ordercancel(), p.ordercancel()));
}

void EClient::reqExecutionsProtoBuf(const protobuf::ExecutionRequest& p)
{
  ExecutionFilter filter;
  if (p.has_executionfilter()) {
    ibx::pb::copy(p.executionfilter(), filter);
    for (int date : p.executionfilter().specificdates())
      filter.m_specificDates.push_back(date);
  }
  reqExecutions(p.has_reqid() ? p.reqid() : NO_VALID_ID, filter);
}

void EClient::reqScannerSubscriptionProtoBuf(const protobuf::ScannerSubscriptionRequest& p)
{
  ScannerSubscription subscription;
  TagValueListSPtr options, filterOptions;
  if (p.has_scannersubscription()) {
    const protobuf::ScannerSubscription& given = p.scannersubscription();
    ibx::pb::copy(given, subscription);
    options = ibx::pb::tagsOf(given.scannersubscriptionoptions());
    filterOptions = ibx::pb::tagsOf(given.scannersubscriptionfilteroptions());
  }
  reqScannerSubscription(p.has_reqid() ? p.reqid() : NO_VALID_ID, subscription, options, filterOptions);
}

void EClient::reqWshEventDataProtoBuf(const protobuf::WshEventDataRequest& p)
{
  WshEventData data;
  ibx::pb::copy(p, data);
  reqWshEventData(p.has_reqid() ? p.reqid() : NO_VALID_ID, data);
}

void EClient::exerciseOptionsProtoBuf(const protobuf::ExerciseOptionsRequest& p)
{
  int id = p.has_orderid() ? p.orderid() : 0;
  // The three fields the client has no counterpart for are refused, not dropped.
  const char* extra = p.has_manualordertime() && !p.manualordertime().empty() ? "manualOrderTime"
                    : p.has_customeraccount() && !p.customeraccount().empty() ? "customerAccount"
                    : p.has_professionalcustomer() && p.professionalcustomer() ? "professionalCustomer"
                    : nullptr;
  if (extra) {
    m_pEWrapper->error(id, Utils::currentTimeMillis(), 321, std::string("exerciseOptionsProtoBuf: ") + extra + " has no counterpart in the client", "");
    return;
  }
  exerciseOptions(id, p.has_contract() ? ibx::pb::contractOf(p.contract()) : Contract(),
                  p.has_exerciseaction() ? p.exerciseaction() : UNSET_INTEGER,
                  p.has_exercisequantity() ? p.exercisequantity() : UNSET_INTEGER, p.has_account() ? p.account() : std::string(),
                  p.has_override() && p.override() ? 1 : 0, std::string(), std::string(), false);
}

// The session is started by eConnect.
void EClient::startApiProtoBuf(const protobuf::StartApiRequest&) {}

// The two configuration requests: the fields of the message that are set
// go to the client as records; the answer is the protobuf callback of the
// request, or an error callback.
void EClient::reqConfigProtoBuf(const protobuf::ConfigRequest& p)
{
  int id = p.has_reqid() ? p.reqid() : NO_VALID_ID;
  if (!ibxReady(id))
    return;
  std::string fields = ibx::pb::recordsOf(p);
  ibxDone(id, ibx_req_config(m_handle, ibx::view(fields)));
}

void EClient::updateConfigProtoBuf(const protobuf::UpdateConfigRequest& p)
{
  int id = p.has_reqid() ? p.reqid() : NO_VALID_ID;
  if (!ibxReady(id))
    return;
  std::string fields = ibx::pb::recordsOf(p);
  ibxDone(id, ibx_update_config(m_handle, ibx::view(fields)));
}
