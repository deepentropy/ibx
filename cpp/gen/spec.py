"""The tables the C++ library and the C entry layer are generated from
(ibx#235): the structures with their fields, the requests and the callbacks.

A structure is a block of member declarations, as a C++ header has them.
After ``;;`` a member takes notes:

    r=<name>    the field of the Rust structure, when it is not the member's
                name in snake case
    x=<key>     a codec written by hand (CUSTOM below)
    none        the member exists in the C++ structure only
    d=in|out    the member crosses in one direction only (in: C++ to Rust)
"""

# name, header, direction (in: C++ to Rust, out: Rust to C++, both, def:
# definition only), Rust type, members, text before the members, text after.
STRUCTS = []


def S(name, header, direction, rust, members, head="", tail=""):
    STRUCTS.append(dict(name=name, header=header, dir=direction, rust=rust, members=members, head=head, tail=tail))


S("TagValue", "TagValue.h", "both", "crate::api::types::TagValue", """
std::string tag
std::string value
""", head="""  TagValue() {}
  TagValue(const std::string& p_tag, const std::string& p_value) : tag(p_tag), value(p_value) {}
""")

S("ComboLeg", "Contract.h", "both", "crate::api::types::ComboLeg", """
int conId = 0
int ratio = 0
std::string action
std::string exchange
int openClose = 0
int shortSaleSlot = 0
std::string designatedLocation
int exemptCode = -1
""", tail="""  bool operator==(const ComboLeg& other) const {
    return conId == other.conId && ratio == other.ratio && openClose == other.openClose
      && shortSaleSlot == other.shortSaleSlot && exemptCode == other.exemptCode && action == other.action
      && exchange == other.exchange && designatedLocation == other.designatedLocation;
  }
""")

S("DeltaNeutralContract", "Contract.h", "both", "crate::api::types::DeltaNeutralContract", """
int conId = 0
double delta = 0
double price = 0
""")

S("Contract", "Contract.h", "both", "crate::api::types::Contract", """
int conId = 0
std::string symbol
std::string secType
std::string lastTradeDateOrContractMonth
std::string lastTradeDate
double strike = UNSET_DOUBLE ;; x=strike
std::string right
std::string multiplier
std::string exchange
std::string primaryExchange
std::string currency
std::string localSymbol
std::string tradingClass
bool includeExpired = false
std::string secIdType
std::string secId
std::string description
std::string issuerId
std::string comboLegsDescrip
ComboLegListSPtr comboLegs ;; splist=ComboLeg
std::unique_ptr<DeltaNeutralContract> deltaNeutralContract ;; uptr=DeltaNeutralContract
""", head="""  typedef std::vector<ComboLegSPtr> ComboLegList;
  typedef std::shared_ptr<ComboLegList> ComboLegListSPtr;
""", tail="""  static void CloneComboLegs(ComboLegListSPtr& dst, const ComboLegListSPtr& src) {
    if (!src.get())
      return;
    dst->reserve(src->size());
    for (const ComboLegSPtr& leg : *src) {
      if (leg)
        dst->push_back(ComboLegSPtr(new ComboLeg(*leg)));
    }
  }
""")

S("ContractDetails", "Contract.h", "out", "crate::api::types::ContractDetails", """
Contract contract ;; struct=Contract
std::string marketName
double minTick = 0
std::string orderTypes
std::string validExchanges
int priceMagnifier = 0
int underConId = 0
std::string longName
std::string contractMonth
std::string industry
std::string category
std::string subcategory
std::string timeZoneId
std::string tradingHours
std::string liquidHours
std::string evRule
double evMultiplier = 0
int aggGroup = UNSET_INTEGER
std::string underSymbol
std::string underSecType
std::string marketRuleIds
std::string realExpirationDate
std::string lastTradeTime
std::string stockType
Decimal minSize = UNSET_DECIMAL
Decimal sizeIncrement = UNSET_DECIMAL
Decimal suggestedSizeIncrement = UNSET_DECIMAL
Decimal minAlgoSize = UNSET_DECIMAL
Decimal lastPricePrecision = UNSET_DECIMAL
Decimal lastSizePrecision = UNSET_DECIMAL
TagValueListSPtr secIdList
std::string cusip
std::string ratings
std::string descAppend
std::string bondType
std::string couponType
bool callable = false
bool putable = false
double coupon = 0
bool convertible = false
std::string maturity
std::string issueDate
std::string nextOptionDate
std::string nextOptionType
bool nextOptionPartial = false
std::string notes
std::string fundName
std::string fundFamily
std::string fundType
std::string fundFrontLoad
std::string fundBackLoad
std::string fundBackLoadTimeInterval
std::string fundManagementFee
bool fundClosed = false
bool fundClosedForNewInvestors = false
bool fundClosedForNewMoney = false
std::string fundNotifyAmount
std::string fundMinimumInitialPurchase
std::string fundSubsequentMinimumPurchase
std::string fundBlueSkyStates
std::string fundBlueSkyTerritories
FundDistributionPolicyIndicator fundDistributionPolicyIndicator = FundDistributionPolicyIndicator::None ;; x=fund_policy
FundAssetType fundAssetType = FundAssetType::None ;; x=fund_asset
IneligibilityReasonListSPtr ineligibilityReasonList ;; splist=IneligibilityReason
std::string eventContract1
std::string eventContractDescription1
std::string eventContractDescription2
std::string settlementMethod ;; none
""")

S("ContractDescription", "Contract.h", "def", "", """
Contract contract
DerivativeSecTypesList derivativeSecTypes
""", head="""  typedef std::vector<std::string> DerivativeSecTypesList;
""")

S("IneligibilityReason", "IneligibilityReason.h", "out", "crate::api::types::IneligibilityReason", """
std::string id
std::string description
""", head="""  IneligibilityReason() {}
  IneligibilityReason(const std::string& p_id, const std::string& p_description) : id(p_id), description(p_description) {}
""")

S("OrderComboLeg", "Order.h", "def", "", """
double price = UNSET_DOUBLE
""", tail="""  bool operator==(const OrderComboLeg& other) const { return price == other.price; }
""")

S("Order", "Order.h", "both", "crate::api::types::Order", """
int orderId = 0
int clientId = 0
long long permId = 0
std::string action
Decimal totalQuantity = UNSET_DECIMAL ;; x=quantity
std::string orderType
double lmtPrice = UNSET_DOUBLE
double auxPrice = UNSET_DOUBLE
std::string tif
std::string activeStartTime
std::string activeStopTime
std::string ocaGroup
int ocaType = 0
std::string orderRef
bool transmit = true
int parentId = 0
bool blockOrder = false
bool sweepToFill = false
int displaySize = 0
int triggerMethod = 0
bool outsideRth = false
bool hidden = false
std::string goodAfterTime
std::string goodTillDate
std::string rule80A ;; r=rule80a
bool allOrNone = false
int minQty = UNSET_INTEGER
double percentOffset = UNSET_DOUBLE
bool overridePercentageConstraints = false
double trailStopPrice = UNSET_DOUBLE
double trailingPercent = UNSET_DOUBLE
std::string faGroup
std::string faMethod
std::string faPercentage
std::string openClose
Origin origin = CUSTOMER ;; enum
int shortSaleSlot = 0
std::string designatedLocation
int exemptCode = -1
double discretionaryAmt = 0
bool optOutSmartRouting = false
int auctionStrategy = AUCTION_UNSET
double startingPrice = UNSET_DOUBLE
double stockRefPrice = UNSET_DOUBLE
double delta = UNSET_DOUBLE
double stockRangeLower = UNSET_DOUBLE
double stockRangeUpper = UNSET_DOUBLE
bool randomizeSize = false
bool randomizePrice = false
double volatility = UNSET_DOUBLE
int volatilityType = UNSET_INTEGER
std::string deltaNeutralOrderType
double deltaNeutralAuxPrice = UNSET_DOUBLE
int deltaNeutralConId = 0
std::string deltaNeutralSettlingFirm
std::string deltaNeutralClearingAccount
std::string deltaNeutralClearingIntent
std::string deltaNeutralOpenClose
bool deltaNeutralShortSale = false
int deltaNeutralShortSaleSlot = 0
std::string deltaNeutralDesignatedLocation
bool continuousUpdate = false
int referencePriceType = UNSET_INTEGER
double basisPoints = UNSET_DOUBLE
int basisPointsType = UNSET_INTEGER
int scaleInitLevelSize = UNSET_INTEGER
int scaleSubsLevelSize = UNSET_INTEGER
double scalePriceIncrement = UNSET_DOUBLE
double scalePriceAdjustValue = UNSET_DOUBLE
int scalePriceAdjustInterval = UNSET_INTEGER
double scaleProfitOffset = UNSET_DOUBLE
bool scaleAutoReset = false
int scaleInitPosition = UNSET_INTEGER
int scaleInitFillQty = UNSET_INTEGER
bool scaleRandomPercent = false
std::string scaleTable
std::string hedgeType
std::string hedgeParam
int hedgeMaxSize = UNSET_INTEGER
std::string account
std::string settlingFirm
std::string clearingAccount
std::string clearingIntent
std::string algoStrategy
TagValueListSPtr algoParams
TagValueListSPtr smartComboRoutingParams
std::string algoId
bool whatIf = false
bool notHeld = false
bool solicited = false
std::string modelCode
OrderComboLegListSPtr orderComboLegs ;; x=order_combo_legs
TagValueListSPtr orderMiscOptions
int referenceContractId = UNSET_INTEGER ;; x=zero_unset_int
double peggedChangeAmount = UNSET_DOUBLE ;; x=zero_unset_double
bool isPeggedChangeAmountDecrease = false
double referenceChangeAmount = UNSET_DOUBLE ;; x=zero_unset_double
std::string referenceExchangeId
std::string adjustedOrderType
double triggerPrice = UNSET_DOUBLE
double adjustedStopPrice = UNSET_DOUBLE
double adjustedStopLimitPrice = UNSET_DOUBLE
double adjustedTrailingAmount = UNSET_DOUBLE
int adjustableTrailingUnit = UNSET_INTEGER ;; x=trailing_unit
double lmtPriceOffset = UNSET_DOUBLE
std::vector<std::shared_ptr<OrderCondition>> conditions ;; x=conditions
bool conditionsCancelOrder = false
bool conditionsIgnoreRth = false
bool conditionsIncludeOvernight = false ;; x=conditions_overnight
std::string extOperator
SoftDollarTier softDollarTier = SoftDollarTier("", "", "") ;; x=soft_dollar_tier
double cashQty = UNSET_DOUBLE
std::string mifid2DecisionMaker
std::string mifid2DecisionAlgo
std::string mifid2ExecutionTrader
std::string mifid2ExecutionAlgo
bool dontUseAutoPriceForHedge = false
bool isOmsContainer = false
bool discretionaryUpToLimitPrice = false
std::string autoCancelDate
Decimal filledQuantity = UNSET_DECIMAL
int refFuturesConId = UNSET_INTEGER ;; x=zero_unset_int
bool autoCancelParent = false
std::string shareholder
bool imbalanceOnly = false
ThreeStateBoolean routeMarketableToBbo = ThreeStateBoolean::STATE_DEFAULT ;; x=three_state
long long parentPermId = UNSET_LLONG ;; x=zero_unset_llong
UsePriceMmgtAlgo usePriceMgmtAlgo = UsePriceMmgtAlgo::DEFAULT ;; enum
int duration = UNSET_INTEGER
int postToAts = UNSET_INTEGER
std::string advancedErrorOverride
std::string manualOrderTime
int minTradeQty = UNSET_INTEGER
int minCompeteSize = UNSET_INTEGER
double competeAgainstBestOffset = UNSET_DOUBLE
double midOffsetAtWhole = UNSET_DOUBLE
double midOffsetAtHalf = UNSET_DOUBLE
std::string customerAccount
bool professionalCustomer = false
std::string bondAccruedInterest
bool includeOvernight = false
int manualOrderIndicator = UNSET_INTEGER
std::string submitter
bool postOnly = false
bool allowPreOpen = false
bool ignoreOpenAuction = false
bool deactivate = false
ThreeStateBoolean seekPriceImprovement = ThreeStateBoolean::STATE_DEFAULT ;; x=three_state
int whatIfType = UNSET_INTEGER
int slOrderId = UNSET_INTEGER
std::string slOrderType
int ptOrderId = UNSET_INTEGER
std::string ptOrderType
""", head="""  typedef std::vector<OrderComboLegSPtr> OrderComboLegList;
  typedef std::shared_ptr<OrderComboLegList> OrderComboLegListSPtr;
""", tail="""  static void CloneOrderComboLegs(OrderComboLegListSPtr& dst, const OrderComboLegListSPtr& src) {
    if (!src.get())
      return;
    dst->reserve(src->size());
    for (const OrderComboLegSPtr& leg : *src) {
      if (leg)
        dst->push_back(OrderComboLegSPtr(new OrderComboLeg(*leg)));
    }
  }
""")

S("OrderAllocation", "OrderState.h", "out", "crate::api::types::OrderAllocation", """
std::string account
Decimal position = UNSET_DECIMAL ;; dectext
Decimal positionDesired = UNSET_DECIMAL ;; dectext
Decimal positionAfter = UNSET_DECIMAL ;; dectext
Decimal desiredAllocQty = UNSET_DECIMAL ;; dectext
Decimal allowedAllocQty = UNSET_DECIMAL ;; dectext
bool isMonetary = false
""", tail="""  bool operator==(const OrderAllocation& other) const { return account == other.account; }
""")

S("OrderState", "OrderState.h", "out", "crate::api::types::OrderState", """
std::string status
std::string initMarginBefore
std::string maintMarginBefore
std::string equityWithLoanBefore
std::string initMarginChange
std::string maintMarginChange
std::string equityWithLoanChange
std::string initMarginAfter
std::string maintMarginAfter
std::string equityWithLoanAfter
double commissionAndFees = UNSET_DOUBLE
double minCommissionAndFees = UNSET_DOUBLE
double maxCommissionAndFees = UNSET_DOUBLE
std::string commissionAndFeesCurrency
std::string marginCurrency
double initMarginBeforeOutsideRTH = UNSET_DOUBLE
double maintMarginBeforeOutsideRTH = UNSET_DOUBLE
double equityWithLoanBeforeOutsideRTH = UNSET_DOUBLE
double initMarginChangeOutsideRTH = UNSET_DOUBLE
double maintMarginChangeOutsideRTH = UNSET_DOUBLE
double equityWithLoanChangeOutsideRTH = UNSET_DOUBLE
double initMarginAfterOutsideRTH = UNSET_DOUBLE
double maintMarginAfterOutsideRTH = UNSET_DOUBLE
double equityWithLoanAfterOutsideRTH = UNSET_DOUBLE
Decimal suggestedSize = UNSET_DECIMAL ;; dectext
std::string rejectReason
OrderAllocationListSPtr orderAllocations ;; splist=OrderAllocation
std::string warningText
std::string completedTime
std::string completedStatus
""")

S("OrderCancel", "OrderCancel.h", "def", "", """
std::string manualOrderCancelTime
std::string extOperator
int manualOrderIndicator = UNSET_INTEGER
""")

S("Execution", "Execution.h", "out", "crate::api::types::Execution", """
std::string execId
std::string time
std::string acctNumber
std::string exchange
std::string side
Decimal shares = ZERO_DECIMAL
double price = 0
long long permId = 0
int clientId = 0
int orderId = 0
int liquidation = 0
Decimal cumQty = ZERO_DECIMAL
double avgPrice = 0
std::string orderRef
std::string evRule
double evMultiplier = 0
std::string modelCode
int lastLiquidity = 0
bool pendingPriceRevision = false
std::string submitter ;; none
OptionExerciseType optExerciseOrLapseType = OptionExerciseType::None ;; none
""")

S("ExecutionFilter", "Execution.h", "in", "crate::api::types::ExecutionFilter", """
int m_clientId = 0 ;; r=client_id
std::string m_acctCode ;; r=acct_code
std::string m_time ;; r=time
std::string m_symbol ;; r=symbol
std::string m_secType ;; r=sec_type
std::string m_exchange ;; r=exchange
std::string m_side ;; r=side
int m_lastNDays = UNSET_INTEGER ;; none
std::list<int> m_specificDates ;; none
""")

S("CommissionAndFeesReport", "CommissionAndFeesReport.h", "out", "crate::api::types::CommissionAndFeesReport", """
std::string execId
double commissionAndFees = 0
std::string currency
double realizedPNL = 0 ;; r=realized_pnl
double yield = 0 ;; r=yield_amount
int yieldRedemptionDate = 0 ;; x=yield_date
""")

S("Bar", "bar.h", "out", "crate::api::types::BarData", """
std::string time ;; r=date
double high = 0
double low = 0
double open = 0
double close = 0
Decimal wap = UNSET_DECIMAL
Decimal volume = UNSET_DECIMAL ;; x=bar_volume
int count = 0 ;; r=bar_count
""")

S("ScannerSubscription", "ScannerSubscription.h", "in", "crate::api::types::ScannerSubscription", """
int numberOfRows = -1
std::string instrument
std::string locationCode
std::string scanCode
double abovePrice = DBL_MAX
double belowPrice = DBL_MAX
int aboveVolume = INT_MAX
double marketCapAbove = DBL_MAX
double marketCapBelow = DBL_MAX
std::string moodyRatingAbove
std::string moodyRatingBelow
std::string spRatingAbove
std::string spRatingBelow
std::string maturityDateAbove
std::string maturityDateBelow
double couponRateAbove = DBL_MAX
double couponRateBelow = DBL_MAX
int excludeConvertible = INT_MAX ;; x=flag_int
int averageOptionVolumeAbove = INT_MAX
std::string scannerSettingPairs
std::string stockTypeFilter
""")

S("WshEventData", "WshEventData.h", "in", "crate::api::types::WshEventData", """
int conId = INT_MAX
std::string filter
bool fillWatchlist = false
bool fillPortfolio = false
bool fillCompetitors = false
std::string startDate
std::string endDate
int totalLimit = INT_MAX
""", tail="""  WshEventData() {}
  WshEventData(int conId_, bool fillWatchlist_, bool fillPortfolio_, bool fillCompetitors_, std::string startDate_,
               std::string endDate_, int totalLimit_)
    : conId(conId_), fillWatchlist(fillWatchlist_), fillPortfolio(fillPortfolio_), fillCompetitors(fillCompetitors_),
      startDate(startDate_), endDate(endDate_), totalLimit(totalLimit_) {}
  WshEventData(std::string filter_, bool fillWatchlist_, bool fillPortfolio_, bool fillCompetitors_, std::string startDate_,
               std::string endDate_, int totalLimit_)
    : filter(filter_), fillWatchlist(fillWatchlist_), fillPortfolio(fillPortfolio_), fillCompetitors(fillCompetitors_),
      startDate(startDate_), endDate(endDate_), totalLimit(totalLimit_) {}
""")

S("DepthMktDataDescription", "DepthMktDataDescription.h", "out", "crate::types::DepthMktDataDescription", """
std::string exchange
std::string secType
std::string listingExch
std::string serviceDataType
int aggGroup = UNSET_INTEGER
""")

S("FamilyCode", "FamilyCode.h", "out", "crate::types::FamilyCode", """
std::string accountID ;; r=account_id
std::string familyCodeStr
""")

S("NewsProvider", "NewsProvider.h", "out", "crate::types::NewsProvider", """
std::string providerCode ;; r=code
std::string providerName ;; r=name
""")

S("PriceIncrement", "PriceIncrement.h", "out", "crate::api::types::PriceIncrement", """
double lowEdge = 0
double increment = 0
""")

S("HistogramEntry", "HistogramEntry.h", "def", "", """
double price = 0
Decimal size = UNSET_DECIMAL
""")

S("HistoricalSession", "HistoricalSession.h", "def", "", """
std::string startDateTime
std::string endDateTime
std::string refDate
""")

S("TickAttrib", "TickAttrib.h", "def", "", """
bool canAutoExecute = false
bool pastLimit = false
bool preOpen = false
""")

S("TickAttribBidAsk", "TickAttribBidAsk.h", "def", "", """
bool bidPastLow = false
bool askPastHigh = false
""")

S("TickAttribLast", "TickAttribLast.h", "def", "", """
bool pastLimit = false
bool unreported = false
""")

S("HistoricalTick", "HistoricalTick.h", "def", "", """
long long time = 0
double price = 0
Decimal size = UNSET_DECIMAL
""")

S("HistoricalTickBidAsk", "HistoricalTickBidAsk.h", "def", "", """
long long time = 0
TickAttribBidAsk tickAttribBidAsk
double priceBid = 0
double priceAsk = 0
Decimal sizeBid = UNSET_DECIMAL
Decimal sizeAsk = UNSET_DECIMAL
""")

S("HistoricalTickLast", "HistoricalTickLast.h", "def", "", """
long long time = 0
TickAttribLast tickAttribLast
double price = 0
Decimal size = UNSET_DECIMAL
std::string exchange
std::string specialConditions
""")

# What a header holds beside its structures: the includes, the text before
# the structures and after them, and the text between two structures
# (keyed by the structure it comes before).
HEADERS = {
    "TagValue.h": dict(includes=["<memory>", "<string>", "<vector>"], after="""typedef std::shared_ptr<TagValue> TagValueSPtr;
typedef std::vector<TagValueSPtr> TagValueList;
typedef std::shared_ptr<TagValueList> TagValueListSPtr;
"""),
    "IneligibilityReason.h": dict(includes=["<memory>", "<string>", "<vector>"], after="""typedef std::shared_ptr<IneligibilityReason> IneligibilityReasonSPtr;
typedef std::vector<IneligibilityReasonSPtr> IneligibilityReasonList;
typedef std::shared_ptr<IneligibilityReasonList> IneligibilityReasonListSPtr;
"""),
    "Contract.h": dict(includes=['"TagValue.h"', '"Decimal.h"', '"CommonDefs.h"', '"IneligibilityReason.h"', "<cfloat>", "<memory>", "<string>", "<vector>"],
                       before="""// The open/close side of a combo leg: as the combo, open, close, unknown.
enum LegOpenClose { SAME_POS, OPEN_POS, CLOSE_POS, UNKNOWN_POS };
""", between={"Contract": "typedef std::shared_ptr<ComboLeg> ComboLegSPtr;\n"}),
    "Order.h": dict(includes=['"CommonDefs.h"', '"TagValue.h"', '"OrderCondition.h"', '"SoftDollarTier.h"', '"Decimal.h"', "<cfloat>", "<climits>", "<cmath>", "<memory>", "<string>", "<vector>"],
                    before="""#define COMPETE_AGAINST_BEST_OFFSET_UP_TO_MID INFINITY

enum Origin { CUSTOMER, FIRM, UNKNOWN };

enum AuctionStrategy { AUCTION_UNSET = 0, AUCTION_MATCH = 1, AUCTION_IMPROVEMENT = 2, AUCTION_TRANSPARENT = 3 };

enum UsePriceMmgtAlgo { DONT_USE = 0, USE, DEFAULT = UNSET_INTEGER };

enum ThreeStateBoolean { STATE_NO = 0, STATE_YES, STATE_DEFAULT = UNSET_INTEGER };
""", between={"Order": "typedef std::shared_ptr<OrderComboLeg> OrderComboLegSPtr;\n"}),
    "OrderState.h": dict(includes=['"Order.h"'], between={"OrderState": """typedef std::shared_ptr<OrderAllocation> OrderAllocationSPtr;
typedef std::vector<OrderAllocationSPtr> OrderAllocationList;
typedef std::shared_ptr<OrderAllocationList> OrderAllocationListSPtr;
"""}),
    "OrderCancel.h": dict(includes=['"CommonDefs.h"', "<climits>", "<string>"]),
    "Execution.h": dict(includes=['"Decimal.h"', '"CommonDefs.h"', "<climits>", "<list>", "<string>"]),
    "CommissionAndFeesReport.h": dict(includes=["<string>"]),
    "bar.h": dict(includes=['"Decimal.h"', "<string>"]),
    "ScannerSubscription.h": dict(includes=["<cfloat>", "<climits>", "<string>"], before="#define NO_ROW_NUMBER_SPECIFIED -1;\n"),
    "WshEventData.h": dict(includes=["<climits>", "<string>"]),
    "DepthMktDataDescription.h": dict(includes=['"CommonDefs.h"', "<climits>", "<string>"]),
    "FamilyCode.h": dict(includes=["<string>"]),
    "NewsProvider.h": dict(includes=["<string>"]),
    "PriceIncrement.h": dict(includes=[]),
    "HistogramEntry.h": dict(includes=['"Decimal.h"']),
    "HistoricalSession.h": dict(includes=["<string>"]),
    "TickAttrib.h": dict(includes=[]),
    "TickAttribBidAsk.h": dict(includes=[]),
    "TickAttribLast.h": dict(includes=[]),
    "HistoricalTick.h": dict(includes=['"Decimal.h"']),
    "HistoricalTickBidAsk.h": dict(includes=['"TickAttribBidAsk.h"', '"Decimal.h"']),
    "HistoricalTickLast.h": dict(includes=['"TickAttribLast.h"', '"Decimal.h"', "<string>"]),
}

# Codecs written by hand: C++ write, C++ read, Rust write, Rust read.
# {c} is the C++ member, {r} the Rust field.
CUSTOM = {
    # The official unset strike is the largest double; the Rust client's is 0.
    "strike": ("w.f({c});", "{c} = r.f();", "w.f({r});", "{{ let v = r.f(); {r} = if v == f64::MAX {{ 0.0 }} else {{ v }}; }}"),
    # The quantity, with its text when the number alone cannot hold it.
    "quantity": ("w.f(ibx::decimalToDoubleOrMax({c})); w.s(ibx::quantityText({c}));",
                 "{{ double d = r.f(); std::string t = r.s(); {c} = t.empty() ? ibx::decimalFromDouble(d) : DecimalFunctions::stringToDecimal(t); }}",
                 "w.f({r}); w.s(&x.total_quantity_text);",
                 "{r} = r.f(); x.total_quantity_text = r.s();"),
    "three_state": ("w.b({c} == STATE_YES);", "{c} = r.b() ? STATE_YES : STATE_NO;", "w.b({r});", "{r} = r.b();"),
    "soft_dollar_tier": ("w.s({c}.name()); w.s({c}.val()); w.s({c}.displayName());",
                         "{{ std::string a = r.s(); std::string b = r.s(); std::string d = r.s(); {c} = SoftDollarTier(a, b, d); }}",
                         "w.s(&x.soft_dollar_tier_name); w.s(&x.soft_dollar_tier_val); w.s(&x.soft_dollar_tier_display_name);",
                         "x.soft_dollar_tier_name = r.s(); x.soft_dollar_tier_val = r.s(); x.soft_dollar_tier_display_name = r.s();"),
    "order_combo_legs": ("ibx::writeLegPrices(w, {c});", "ibx::readLegPrices(r, {c});",
                         "w.n({r}.len()); for p in &{r} {{ w.f(*p); }}",
                         "{{ let n = r.n(); {r} = (0..n).map(|_| r.f()).collect(); }}"),
    "conditions": ("ibx::writeConditions(w, {c});", "ibx::readConditions(r, {c});",
                   "crate::capi::write_conditions(w, &{r});", "{r} = crate::capi::read_conditions(r);"),
    # No counterpart in the client: an order that sets it is refused.
    "conditions_overnight": ("w.b({c});", "(void)r.b();", "w.b(false);",
                             "if r.b() {{ r.refuse(10340, \"The following order attribute is not supported: ConditionsIncludeOvernight\"); }}"),
    # The client holds "none" as 0 where the C++ structure starts unset: a
    # report of the reference leaves such a field out, and the official
    # client then keeps the unset value.
    "zero_unset_int": ("w.i({c} == UNSET_INTEGER ? 0 : {c});", "{{ long long v = r.i(); {c} = v == 0 ? UNSET_INTEGER : (int)v; }}",
                       "w.i({r} as i64);", "{r} = r.i() as _;"),
    "zero_unset_llong": ("w.i({c} == UNSET_LLONG ? 0 : {c});", "{{ long long v = r.i(); {c} = v == 0 ? UNSET_LLONG : v; }}",
                         "w.i({r} as i64);", "{r} = r.i() as _;"),
    "zero_unset_double": ("w.f({c} == UNSET_DOUBLE ? 0 : {c});", "{{ double v = r.f(); {c} = v == 0 ? UNSET_DOUBLE : v; }}",
                          "w.f({r});", "{r} = r.f();"),
    # The unit of an adjusted trailing amount: a report of the reference
    # always carries it, 0 for none.
    "trailing_unit": ("w.i({c} == UNSET_INTEGER ? 0 : {c});", "{{ long long v = r.i(); {c} = v == UNSET_INTEGER ? 0 : (int)v; }}",
                      "w.i({r} as i64);", "{r} = r.i() as _;"),
    "fund_policy": ("", "{c} = Utils::getFundDistributionPolicyIndicator(r.s());", "w.s(&{r});", ""),
    "fund_asset": ("", "{c} = Utils::getFundAssetType(r.s());", "w.s(&{r});", ""),
    "yield_date": ("", "{c} = (int)r.i();", "w.i({r}.trim().parse::<i64>().unwrap_or(0));", ""),
    "bar_volume": ("", "{c} = ibx::decimalFromDouble(r.f());", "w.f({r} as f64);", ""),
    "flag_int": ("w.b({c} != 0 && {c} != INT_MAX);", "", "", "{r} = r.b();"),
}

# ── Requests ──
# The official name, its parameters, the call on the Rust client. In the
# call, `c` is the client, `w` a wrapper that takes nothing, and each
# parameter has its name in snake case. Only the parameters the call names
# cross the boundary.
REQUESTS = []


def Q(name, params, call, ext=False):
    REQUESTS.append(dict(name=name, params=params, call=call, ext=ext))


Q("reqMktData", "int reqId, const Contract& contract, const std::string& genericTicks, bool snapshot, bool regulatorySnaphsot, const TagValueListSPtr& mktDataOptions",
  "c.req_mkt_data(req_id, &contract, generic_ticks, snapshot, regulatory_snaphsot)")
Q("cancelMktData", "int reqId", "c.cancel_mkt_data(req_id)")
Q("placeOrder", "int id, const Contract& contract, const Order& order", "c.place_order(id, &contract, &order)")
Q("cancelOrder", "int id, const OrderCancel& orderCancel", "c.cancel_order(id, \"\")")
Q("reqOpenOrders", "", "c.req_open_orders(w)")
Q("reqAccountUpdates", "bool subscribe, const std::string& acctCode", "c.req_account_updates(subscribe, acct_code)")
Q("reqExecutions", "int reqId, const ExecutionFilter& filter", "c.req_executions(req_id, &filter, w)")
Q("reqIds", "int numIds", "c.req_ids(w)")
Q("reqContractDetails", "int reqId, const Contract& contract", "c.req_contract_details(req_id, &contract)")
Q("reqMktDepth", "int reqId, const Contract& contract, int numRows, bool isSmartDepth, const TagValueListSPtr& mktDepthOptions",
  "c.req_mkt_depth(req_id, &contract, num_rows as i32, is_smart_depth)")
Q("cancelMktDepth", "int reqId, bool isSmartDepth", "c.cancel_mkt_depth(req_id)")
Q("reqNewsBulletins", "bool allMsgs", "c.req_news_bulletins(all_msgs)")
Q("cancelNewsBulletins", "", "c.cancel_news_bulletins()")
Q("setServerLogLevel", "int level", "c.set_server_log_level(level as i32)")
Q("reqAutoOpenOrders", "bool bAutoBind", "c.req_auto_open_orders(b_auto_bind)")
Q("reqAllOpenOrders", "", "c.req_all_open_orders(w)")
Q("reqManagedAccts", "", "c.req_managed_accts(w)")
Q("requestFA", "faDataType pFaDataType", "c.request_fa(p_fa_data_type as i32)")
Q("replaceFA", "int reqId, faDataType pFaDataType, const std::string& cxml", "c.replace_fa(req_id, p_fa_data_type as i32, cxml)")
Q("reqHistoricalData", "int reqId, const Contract& contract, const std::string& endDateTime, const std::string& durationStr, const std::string& barSizeSetting, const std::string& whatToShow, int useRTH, int formatDate, bool keepUpToDate, const TagValueListSPtr& chartOptions",
  "c.req_historical_data(req_id, &contract, end_date_time, duration_str, bar_size_setting, what_to_show, use_rth != 0, format_date as i32, keep_up_to_date)")
Q("exerciseOptions", "int reqId, const Contract& contract, int exerciseAction, int exerciseQuantity, const std::string& account, int override, const std::string& manualOrderTime, const std::string& customerAccount, bool professionalCustomer",
  "c.exercise_options(req_id, &contract, exercise_action as i32, exercise_quantity as i32, account, override_ as i32)")
Q("cancelHistoricalData", "int reqId", "c.cancel_historical_data(req_id)")
Q("reqRealTimeBars", "int reqId, const Contract& contract, int barSize, const std::string& whatToShow, bool useRTH, const TagValueListSPtr& realTimeBarsOptions",
  "c.req_real_time_bars(req_id, &contract, bar_size as i32, what_to_show, use_rth)")
Q("cancelRealTimeBars", "int reqId", "c.cancel_real_time_bars(req_id)")
Q("cancelScannerSubscription", "int reqId", "c.cancel_scanner_subscription(req_id)")
Q("reqScannerParameters", "", "c.req_scanner_parameters()")
Q("reqScannerSubscription", "int reqId, const ScannerSubscription& subscription, const TagValueListSPtr& scannerSubscriptionOptions, const TagValueListSPtr& scannerSubscriptionFilterOptions",
  "c.req_scanner_subscription(req_id, &subscription, &scanner_subscription_options, &scanner_subscription_filter_options)")
Q("reqCurrentTime", "", "c.req_current_time(w)")
Q("calculateImpliedVolatility", "int reqId, const Contract& contract, double optionPrice, double underPrice, const TagValueListSPtr& miscOptions",
  "c.calculate_implied_volatility(req_id, &contract, option_price, under_price, &misc_options)")
Q("calculateOptionPrice", "int reqId, const Contract& contract, double volatility, double underPrice, const TagValueListSPtr& miscOptions",
  "c.calculate_option_price(req_id, &contract, volatility, under_price, &misc_options)")
Q("cancelCalculateImpliedVolatility", "int reqId", "c.cancel_calculate_implied_volatility(req_id)")
Q("cancelCalculateOptionPrice", "int reqId", "c.cancel_calculate_option_price(req_id)")
Q("reqGlobalCancel", "const OrderCancel& orderCancel", "c.req_global_cancel()")
Q("reqMarketDataType", "int marketDataType", "c.req_market_data_type(market_data_type as i32)")
Q("reqPositions", "", "c.req_positions(w)")
Q("cancelPositions", "", "c.cancel_positions()")
Q("reqAccountSummary", "int reqId, const std::string& groupName, const std::string& tags", "c.req_account_summary(req_id, group_name, tags)")
Q("cancelAccountSummary", "int reqId", "c.cancel_account_summary(req_id)")
Q("verifyRequest", "const std::string& apiName, const std::string& apiVersion", "c.verify_request(api_name, api_version)")
Q("verifyMessage", "const std::string& apiData", "c.verify_message(api_data)")
Q("verifyAndAuthRequest", "const std::string& apiName, const std::string& apiVersion, const std::string& opaqueIsvKey",
  "c.verify_and_auth_request(api_name, api_version, opaque_isv_key)")
Q("verifyAndAuthMessage", "const std::string& apiData, const std::string& xyzResponse", "c.verify_and_auth_message(api_data, xyz_response)")
Q("queryDisplayGroups", "int reqId", "c.query_display_groups(req_id, w)")
Q("subscribeToGroupEvents", "int reqId, int groupId", "c.subscribe_to_group_events(req_id, group_id as i32, w)")
Q("updateDisplayGroup", "int reqId, const std::string& contractInfo", "c.update_display_group(req_id, contract_info)")
Q("unsubscribeFromGroupEvents", "int reqId", "c.unsubscribe_from_group_events(req_id)")
Q("reqPositionsMulti", "int reqId, const std::string& account, const std::string& modelCode", "c.req_positions_multi(req_id, account, model_code, w)")
Q("cancelPositionsMulti", "int reqId", "c.cancel_positions_multi(req_id)")
Q("reqAccountUpdatesMulti", "int reqId, const std::string& account, const std::string& modelCode, bool ledgerAndNLV",
  "c.req_account_updates_multi(req_id, account, model_code, ledger_and_nlv, w)")
Q("cancelAccountUpdatesMulti", "int reqId", "c.cancel_account_updates_multi(req_id)")
Q("reqSecDefOptParams", "int reqId, const std::string& underlyingSymbol, const std::string& futFopExchange, const std::string& underlyingSecType, int underlyingConId",
  "c.req_sec_def_opt_params(req_id, underlying_symbol, fut_fop_exchange, underlying_sec_type, underlying_con_id)")
Q("reqSoftDollarTiers", "int reqId", "c.req_soft_dollar_tiers(req_id, w)")
Q("reqFamilyCodes", "", "c.req_family_codes(w)")
Q("reqMatchingSymbols", "int reqId, const std::string& pattern", "c.req_matching_symbols(req_id, pattern)")
Q("reqMktDepthExchanges", "", "c.req_mkt_depth_exchanges()")
Q("reqSmartComponents", "int reqId, std::string bboExchange", "c.req_smart_components(req_id, bbo_exchange, w)")
Q("reqNewsProviders", "", "c.req_news_providers(w)")
Q("reqNewsArticle", "int requestId, const std::string& providerCode, const std::string& articleId, const TagValueListSPtr& newsArticleOptions",
  "c.req_news_article(request_id, provider_code, article_id)")
Q("reqHistoricalNews", "int requestId, int conId, const std::string& providerCodes, const std::string& startDateTime, const std::string& endDateTime, int totalResults, const TagValueListSPtr& historicalNewsOptions",
  "c.req_historical_news(request_id, con_id, provider_codes, start_date_time, end_date_time, total_results.max(0) as u32)")
Q("reqHeadTimestamp", "int reqId, const Contract& contract, const std::string& whatToShow, int useRTH, int formatDate",
  "c.req_head_time_stamp(req_id, &contract, what_to_show, use_rth != 0, format_date as i32)")
Q("cancelHeadTimestamp", "int reqId", "c.cancel_head_time_stamp(req_id)")
Q("reqHistogramData", "int reqId, const Contract& contract, bool useRTH, const std::string& timePeriod",
  "c.req_histogram_data(req_id, &contract, use_rth, time_period)")
Q("cancelHistogramData", "int reqId", "c.cancel_histogram_data(req_id)")
Q("reqMarketRule", "int marketRuleId", "c.req_market_rule(market_rule_id as i32, w)")
Q("reqPnL", "int reqId, const std::string& account, const std::string& modelCode", "c.req_pnl(req_id, account, model_code)")
Q("cancelPnL", "int reqId", "c.cancel_pnl(req_id)")
Q("reqPnLSingle", "int reqId, const std::string& account, const std::string& modelCode, int conId", "c.req_pnl_single(req_id, account, model_code, con_id)")
Q("cancelPnLSingle", "int reqId", "c.cancel_pnl_single(req_id)")
Q("reqHistoricalTicks", "int reqId, const Contract& contract, const std::string& startDateTime, const std::string& endDateTime, int numberOfTicks, const std::string& whatToShow, int useRth, bool ignoreSize, const TagValueListSPtr& miscOptions",
  "c.req_historical_ticks(req_id, &contract, start_date_time, end_date_time, number_of_ticks as i32, what_to_show, use_rth != 0, ignore_size, &misc_options)")
Q("reqTickByTickData", "int reqId, const Contract& contract, const std::string& tickType, int numberOfTicks, bool ignoreSize",
  "c.req_tick_by_tick_data(req_id, &contract, tick_type, number_of_ticks as i32, ignore_size)")
Q("cancelTickByTickData", "int reqId", "c.cancel_tick_by_tick_data(req_id)")
Q("reqCompletedOrders", "bool apiOnly", "c.req_completed_orders_only(api_only)")
Q("reqWshMetaData", "int reqId", "c.req_wsh_meta_data(req_id)")
Q("reqWshEventData", "int reqId, const WshEventData& wshEventData", "c.req_wsh_event_data(req_id, &wsh_event_data)")
Q("cancelWshMetaData", "int reqId", "c.cancel_wsh_meta_data(req_id)")
Q("cancelWshEventData", "int reqId", "c.cancel_wsh_event_data(req_id)")
Q("reqUserInfo", "int reqId", "c.req_user_info(req_id, w)")
Q("reqCurrentTimeInMillis", "", "c.req_current_time_in_millis(w)")
Q("cancelContractData", "int reqId", "c.cancel_contract_data(req_id)")
Q("cancelHistoricalTicks", "int reqId", "c.cancel_historical_ticks(req_id)")
# Requests of the client the official list no longer has.
Q("reqFundamentalData", "int reqId, const Contract& contract, const std::string& reportType, const TagValueListSPtr& fundamentalDataOptions",
  "c.req_fundamental_data(req_id, &contract, report_type)", ext=True)
Q("cancelFundamentalData", "int reqId", "c.cancel_fundamental_data(req_id)", ext=True)

# ── Callbacks ──
# The official name and parameters. `rust`: the callback of the Rust
# wrapper (the name in snake case when not given, None when the client
# never gives it). `args`: for a parameter that is not the Rust parameter
# at the same place read in the plain way:
#   ("cpp", expr)             made by the C++ side, nothing crosses
#   ("buf", enc, dec, type)   a Rust encoder and a C++ decoder by hand
#   ("at", n)                 the Rust parameter number n
CALLBACKS = []


def C(name, params, rust="", args=None, ext=False, raw=None):
    CALLBACKS.append(dict(name=name, params=params, rust=rust, args=args or {}, ext=ext, raw=raw))


C("tickPrice", "int reqId, TickType field, double price, const TickAttrib& attrib")
C("tickSize", "int reqId, TickType field, Decimal size")
C("tickOptionComputation", "int reqId, TickType tickType, int tickAttrib, double impliedVol, double delta, double optPrice, double pvDividend, double gamma, double vega, double theta, double undPrice")
C("tickGeneric", "int reqId, TickType tickType, double value")
C("tickString", "int reqId, TickType tickType, const std::string& value")
C("tickEFP", "int reqId, TickType tickType, double basisPoints, const std::string& formattedBasisPoints, double totalDividends, int holdDays, const std::string& futureLastTradeDate, double dividendImpact, double dividendsToLastTradeDate", rust=None)
C("orderStatus", "int orderId, const std::string& status, Decimal filled, Decimal remaining, double avgFillPrice, long long permId, int parentId, double lastFillPrice, int clientId, const std::string& whyHeld, double mktCapPrice")
C("openOrder", "int orderId, const Contract& contract, const Order& order, const OrderState& orderState")
C("openOrderEnd", "")
C("winError", "const std::string& str, int lastError", rust=None)
C("connectionClosed", "")
C("updateAccountValue", "const std::string& key, const std::string& val, const std::string& currency, const std::string& accountName")
C("updatePortfolio", "const Contract& contract, Decimal position, double marketPrice, double marketValue, double averageCost, double unrealizedPNL, double realizedPNL, const std::string& accountName")
C("updateAccountTime", "const std::string& timeStamp")
C("accountDownloadEnd", "const std::string& accountName")
C("nextValidId", "int orderId")
C("contractDetails", "int reqId, const ContractDetails& contractDetails")
C("bondContractDetails", "int reqId, const ContractDetails& contractDetails")
C("contractDetailsEnd", "int reqId")
C("execDetails", "int reqId, const Contract& contract, const Execution& execution")
C("execDetailsEnd", "int reqId")
C("error", "int id, long long errorTimeMs, int errorCode, const std::string& errorString, const std::string& advancedOrderRejectJson",
  args={"errorTimeMs": ("cpp", "Utils::currentTimeMillis()"), "errorCode": ("at", 1), "errorString": ("at", 2), "advancedOrderRejectJson": ("at", 3)})
C("updateMktDepth", "int reqId, int position, int operation, int side, double price, Decimal size")
C("updateMktDepthL2", "int reqId, int position, const std::string& marketMaker, int operation, int side, double price, Decimal size, bool isSmartDepth")
C("updateNewsBulletin", "int msgId, int msgType, const std::string& newsMessage, const std::string& originExch")
C("managedAccounts", "const std::string& accountsList")
C("receiveFA", "faDataType pFaDataType, const std::string& cxml")
C("historicalData", "int reqId, const Bar& bar")
C("historicalDataEnd", "int reqId, const std::string& startDateStr, const std::string& endDateStr")
C("scannerParameters", "const std::string& xml")
C("scannerData", "int reqId, int rank, const ContractDetails& contractDetails, const std::string& distance, const std::string& benchmark, const std::string& projection, const std::string& legsStr")
C("scannerDataEnd", "int reqId")
C("realtimeBar", "int reqId, long long time, double open, double high, double low, double close, Decimal volume, Decimal wap, int count", rust="real_time_bar")
C("currentTime", "long long time")
C("deltaNeutralValidation", "int reqId, const DeltaNeutralContract& deltaNeutralContract",
  raw=dict(abi="int64_t req_id, int64_t con_id, double delta, double price",
           rust="req_id, con_id, delta, price",
           cpp="DeltaNeutralContract d; d.conId = (int)con_id; d.delta = delta; d.price = price; s->w->deltaNeutralValidation((int)req_id, d);",
           check="reqId == (int)tvI(\"req_id\", 0) && deltaNeutralContract.conId == (int)tvI(\"con_id\", 0) && deltaNeutralContract.delta == tvF(\"delta\", 0) && deltaNeutralContract.price == tvF(\"price\", 0)"))
C("tickSnapshotEnd", "int reqId")
C("marketDataType", "int reqId, int marketDataType")
C("commissionAndFeesReport", "const CommissionAndFeesReport& commissionAndFeesReport")
C("position", "const std::string& account, const Contract& contract, Decimal position, double avgCost")
C("positionEnd", "")
C("accountSummary", "int reqId, const std::string& account, const std::string& tag, const std::string& value, const std::string& currency")
C("accountSummaryEnd", "int reqId")
C("verifyMessageAPI", "const std::string& apiData", rust="verify_message_api")
C("verifyCompleted", "bool isSuccessful, const std::string& errorText")
C("displayGroupList", "int reqId, const std::string& groups")
C("displayGroupUpdated", "int reqId, const std::string& contractInfo")
C("verifyAndAuthMessageAPI", "const std::string& apiData, const std::string& xyzChallange", rust="verify_and_auth_message_api")
C("verifyAndAuthCompleted", "bool isSuccessful, const std::string& errorText")
C("connectAck", "")
C("positionMulti", "int reqId, const std::string& account, const std::string& modelCode, const Contract& contract, Decimal pos, double avgCost")
C("positionMultiEnd", "int reqId")
C("accountUpdateMulti", "int reqId, const std::string& account, const std::string& modelCode, const std::string& key, const std::string& value, const std::string& currency")
C("accountUpdateMultiEnd", "int reqId")
C("securityDefinitionOptionalParameter", "int reqId, const std::string& exchange, int underlyingConId, const std::string& tradingClass, const std::string& multiplier, const std::set<std::string>& expirations, const std::set<double>& strikes",
  rust="security_definition_option_parameter",
  args={"expirations": ("buf", "crate::capi::enc_texts", "ibx::decTextSet", "std::set<std::string>"),
        "strikes": ("buf", "crate::capi::enc_doubles", "ibx::decDoubleSet", "std::set<double>")})
C("securityDefinitionOptionalParameterEnd", "int reqId", rust="security_definition_option_parameter_end")
C("softDollarTiers", "int reqId, const std::vector<SoftDollarTier>& tiers",
  args={"tiers": ("buf", "crate::capi::enc_soft_dollar_tiers", "ibx::decSoftDollarTiers", "std::vector<SoftDollarTier>")})
C("familyCodes", "const std::vector<FamilyCode>& familyCodes")
C("symbolSamples", "int reqId, const std::vector<ContractDescription>& contractDescriptions",
  args={"contractDescriptions": ("buf", "crate::capi::enc_contract_descriptions", "ibx::decContractDescriptions", "std::vector<ContractDescription>")})
C("mktDepthExchanges", "const std::vector<DepthMktDataDescription>& depthMktDataDescriptions")
C("tickNews", "int reqId, long long timeStampMs, const std::string& providerCode, const std::string& articleId, const std::string& headline, const std::string& extraData")
C("smartComponents", "int reqId, const SmartComponentsMap& theMap",
  args={"theMap": ("buf", "crate::capi::enc_smart_components", "ibx::decSmartComponents", "SmartComponentsMap")})
C("tickReqParams", "int reqId, double minTick, const std::string& bboExchange, int snapshotPermissions")
C("newsProviders", "const std::vector<NewsProvider>& newsProviders")
C("newsArticle", "int requestId, int articleType, const std::string& articleText")
C("historicalNews", "int requestId, const std::string& time, const std::string& providerCode, const std::string& articleId, const std::string& headline")
C("historicalNewsEnd", "int requestId, bool hasMore")
C("headTimestamp", "int reqId, const std::string& headTimestamp")
C("histogramData", "int reqId, const HistogramDataVector& data",
  args={"data": ("buf", "crate::capi::enc_histogram", "ibx::decHistogram", "HistogramDataVector")})
C("historicalDataUpdate", "int reqId, const Bar& bar")
C("rerouteMktDataReq", "int reqId, int conid, const std::string& exchange", rust=None)
C("rerouteMktDepthReq", "int reqId, int conid, const std::string& exchange", rust=None)
C("marketRule", "int marketRuleId, const std::vector<PriceIncrement>& priceIncrements")
C("pnl", "int reqId, double dailyPnL, double unrealizedPnL, double realizedPnL")
C("pnlSingle", "int reqId, Decimal pos, double dailyPnL, double unrealizedPnL, double realizedPnL, double value")
C("historicalTicks", "int reqId, const std::vector<HistoricalTick>& ticks, bool done",
  args={"ticks": ("buf", "crate::capi::enc_ticks", "ibx::decTicks", "std::vector<HistoricalTick>")})
C("historicalTicksBidAsk", "int reqId, const std::vector<HistoricalTickBidAsk>& ticks, bool done",
  args={"ticks": ("buf", "crate::capi::enc_ticks", "ibx::decTicksBidAsk", "std::vector<HistoricalTickBidAsk>")})
C("historicalTicksLast", "int reqId, const std::vector<HistoricalTickLast>& ticks, bool done",
  args={"ticks": ("buf", "crate::capi::enc_ticks", "ibx::decTicksLast", "std::vector<HistoricalTickLast>")})
C("tickByTickAllLast", "int reqId, int tickType, long long time, double price, Decimal size, const TickAttribLast& tickAttribLast, const std::string& exchange, const std::string& specialConditions")
C("tickByTickBidAsk", "int reqId, long long time, double bidPrice, double askPrice, Decimal bidSize, Decimal askSize, const TickAttribBidAsk& tickAttribBidAsk")
C("tickByTickMidPoint", "int reqId, long long time, double midPoint")
C("orderBound", "long long permId, int clientId, int orderId", rust=None)
C("completedOrder", "const Contract& contract, const Order& order, const OrderState& orderState")
C("completedOrdersEnd", "")
C("replaceFAEnd", "int reqId, const std::string& text", rust="replace_fa_end")
C("wshMetaData", "int reqId, const std::string& dataJson")
C("wshEventData", "int reqId, const std::string& dataJson")
C("historicalSchedule", "int reqId, const std::string& startDateTime, const std::string& endDateTime, const std::string& timeZone, const std::vector<HistoricalSession>& sessions",
  args={"sessions": ("buf", "crate::capi::enc_sessions", "ibx::decSessions", "std::vector<HistoricalSession>")})
C("userInfo", "int reqId, const std::string& whiteBrandingId")
C("currentTimeInMillis", "long long timeInMillis")
# A callback of the client the official list no longer has.
C("fundamentalData", "int reqId, const std::string& data", ext=True)

# ── Tests ──
# The value a test gives to a callback parameter whose Rust type has no
# plain rule: the Rust expression, and the C++ condition on the parameter
# `v` that must hold.
FIRE = {
    "&TickAttrib": ("&TickAttrib { can_auto_execute: true, past_limit: false, pre_open: true }",
                    "v.canAutoExecute && !v.pastLimit && v.preOpen"),
    "&TickAttribLast": ("&TickAttribLast { past_limit: false, unreported: true }", "!v.pastLimit && v.unreported"),
    "&TickAttribBidAsk": ("&TickAttribBidAsk { bid_past_low: true, ask_past_high: false }", "v.bidPastLow && !v.askPastHigh"),
    "&[String]": ("&[\"e1\".to_string(), \"e2\".to_string()]", "v.size() == 2 && v.count(\"e1\") && v.count(\"e2\")"),
    "&[f64]": ("&[1.5, 2.5]", "v.size() == 2 && v.count(1.5) && v.count(2.5)"),
    "&[(f64, i64)]": ("&[(1.5, 2), (2.5, 3)]",
                      "v.size() == 2 && v[0].price == 1.5 && ibx::decimalToDoubleOrMax(v[0].size) == 2 && v[1].price == 2.5 && ibx::decimalToDoubleOrMax(v[1].size) == 3"),
    # A session of a schedule: its day, its start, its end.
    "&[(String, String, String)]": ("&[(\"day1\".to_string(), \"start1\".to_string(), \"end1\".to_string()), (\"day2\".to_string(), \"start2\".to_string(), \"end2\".to_string())]",
                                    "v.size() == 2 && v[0].refDate == \"day1\" && v[0].startDateTime == \"start1\" && v[0].endDateTime == \"end1\" && v[1].refDate == \"day2\""),
    "&[crate::types::SmartComponent]": ("&[crate::types::SmartComponent { bit_number: 3, exchange: \"EX3\".into(), exchange_letter: \"K\".into() }]",
                                        "v.size() == 1 && v.count(3) && std::get<0>(v.at(3)) == \"EX3\" && std::get<1>(v.at(3)) == 'K'"),
    "&[crate::types::SoftDollarTier]": ("&[crate::types::SoftDollarTier { name: \"n\".into(), val: \"v\".into(), display_name: \"d\".into() }]",
                                        "v.size() == 1 && v[0].name() == \"n\" && v[0].val() == \"v\" && v[0].displayName() == \"d\""),
    "&[ContractDescription]": ("&[ContractDescription { con_id: 7, symbol: \"SYM\".into(), sec_type: \"STK\".into(), currency: \"USD\".into(), primary_exchange: \"PX\".into(), derivative_sec_types: vec![\"OPT\".into(), \"WAR\".into()], description: \"DESC\".into(), issuer_id: \"ISS\".into() }]",
                               "v.size() == 1 && v[0].contract.conId == 7 && v[0].contract.symbol == \"SYM\" && v[0].contract.secType == \"STK\" && v[0].contract.currency == \"USD\" && v[0].contract.primaryExchange == \"PX\" && v[0].contract.description == \"DESC\" && v[0].contract.issuerId == \"ISS\" && v[0].derivativeSecTypes.size() == 2 && v[0].derivativeSecTypes[1] == \"WAR\""),
}

# The same, for one parameter of one callback.
FIRE_AT = {
    ("historical_ticks", "ticks"): (
        "&HistoricalTickData::Midpoint(vec![crate::types::HistoricalTickMidpoint { time: 11, price: 1.5, size: 2.0 }])",
        "v.size() == 1 && v[0].time == 11 && v[0].price == 1.5 && ibx::decimalToDoubleOrMax(v[0].size) == 2"),
    ("historical_ticks_bid_ask", "ticks"): (
        "&HistoricalTickData::BidAsk(vec![crate::types::HistoricalTickBidAsk { time: 12, tick_attrib_bid_ask: TickAttribBidAsk { bid_past_low: true, ask_past_high: false }, price_bid: 1.5, price_ask: 2.5, size_bid: 3.0, size_ask: 4.0 }])",
        "v.size() == 1 && v[0].time == 12 && v[0].tickAttribBidAsk.bidPastLow && !v[0].tickAttribBidAsk.askPastHigh && v[0].priceBid == 1.5 && v[0].priceAsk == 2.5 && ibx::decimalToDoubleOrMax(v[0].sizeBid) == 3 && ibx::decimalToDoubleOrMax(v[0].sizeAsk) == 4"),
    ("historical_ticks_last", "ticks"): (
        "&HistoricalTickData::Last(vec![crate::types::HistoricalTickLast { time: 13, tick_attrib_last: TickAttribLast { past_limit: true, unreported: false }, price: 1.5, size: 2.0, exchange: \"EX\".into(), special_conditions: \"SC\".into() }])",
        "v.size() == 1 && v[0].time == 13 && v[0].tickAttribLast.pastLimit && !v[0].tickAttribLast.unreported && v[0].price == 1.5 && ibx::decimalToDoubleOrMax(v[0].size) == 2 && v[0].exchange == \"EX\" && v[0].specialConditions == \"SC\""),
}
