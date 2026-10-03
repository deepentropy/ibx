"""ibx#470: combo (BAG) orders from Python. The legs, the per-leg prices and
the smartComboRoutingParams reach the engine (from this module's ComboLeg and
OrderComboLeg, or from objects of the official API's shape); the refusals of
the reference come before anything is sent; openOrder shows the combo with
its legs."""

from types import SimpleNamespace

from ibx import ComboLeg, Contract, EClient, EWrapper, Order, OrderComboLeg, TagValue

SPY, QQQ, SMART_COMBO = 756733, 320227571, 28812380


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.errors = []
        self.open = []

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.errors.append((req_id, error_code, error_string))

    def open_order(self, order_id, contract, order, order_state):
        self.open.append((order_id, contract, order, order_state.status))


def connected():
    w = Recorder()
    c = EClient(w)
    c._test_connect("DUXXXXXXX")
    c._test_set_smart_combo_con_ids("EUR:58666491,USD:28812380")
    c._test_seed_instrument(SMART_COMBO, 0)
    return c, w


def combo(legs):
    c = Contract()
    c.symbol, c.secType, c.exchange, c.currency = "QQQ,SPY", "BAG", "SMART", "USD"
    c.comboLegs = legs
    return c


def leg(con_id, action):
    l = ComboLeg()
    l.conId, l.ratio, l.action, l.exchange = con_id, 1, action, "SMART"
    return l


def order(**kw):
    o = Order()
    o.action, o.totalQuantity, o.orderType = "BUY", 1, "LMT"
    o.lmtPrice = kw.get("lmt", -23.15)
    o.smartComboRoutingParams = [TagValue("NonGuaranteed", "1")]
    if "prices" in kw:
        o.orderComboLegs = [OrderComboLeg(p) for p in kw["prices"]]
    return o


def test_combo_classes_keep_their_fields():
    l = ComboLeg(SPY, 1, "BUY", "SMART")
    assert (l.conId, l.ratio, l.action, l.exchange, l.openClose, l.shortSaleSlot, l.exemptCode) == (SPY, 1, "BUY", "SMART", 0, 0, -1)
    c = combo([leg(SPY, "BUY"), leg(QQQ, "SELL")])
    assert [x.conId for x in c.comboLegs] == [SPY, QQQ]
    o = order(prices=[721.35, 794.5])
    assert [x.price for x in o.orderComboLegs] == [721.35, 794.5]
    assert OrderComboLeg().price > 1e300


def test_combo_reaches_the_engine():
    c, w = connected()
    c.place_order(5, combo([leg(SPY, "BUY"), leg(QQQ, "SELL")]), order())
    c._test_dispatch_once()
    assert w.errors == []
    assert c._test_take_combo() == ("SMART", "USD", "QQQ,SPY", SMART_COMBO, [(SPY, 1, True), (QQQ, 1, False)], [], [(6248, "1")])


def test_legs_of_the_official_api_shape_and_leg_prices():
    c, w = connected()
    legs = [SimpleNamespace(conId=SPY, ratio=1, action="BUY", exchange="SMART", openClose=0, shortSaleSlot=0, designatedLocation="", exemptCode=-1),
            SimpleNamespace(conId=QQQ, ratio=1, action="SELL", exchange="SMART", openClose=0, shortSaleSlot=0, designatedLocation="", exemptCode=-1)]
    o = order(lmt=1.7976931348623157e308, prices=[721.35, 794.5])
    o.orderComboLegs = [SimpleNamespace(price=721.35), SimpleNamespace(price=794.5)]
    c.place_order(6, combo(legs), o)
    c._test_dispatch_once()
    assert w.errors == []
    taken = c._test_take_combo()
    assert taken[4] == [(SPY, 1, True), (QQQ, 1, False)]
    assert taken[5] == [721.35, 794.5]


def test_refusals_before_sending():
    c, w = connected()
    c.place_order(7, combo([]), order())
    no_smart = combo([leg(SPY, "BUY"), leg(QQQ, "SELL")])
    no_smart.currency = "CHF"
    c.place_order(8, no_smart, order())
    c.place_order(9, combo([leg(SPY, "BUY"), leg(QQQ, "SELL")]), order(prices=[1.0, 2.0]))
    c._test_dispatch_once()
    assert w.errors == [
        (7, 321, "Error validating request.-'bH' : cause - Security type 'BAG' requires combo leg details."),
        (8, 321, "Error validating request.-'bH' : cause - Currency CHF isn't supported for smart combo."),
        (9, 321, "Error validating request.-'bH' : cause - Can't specify combo price when using per-leg prices."),
    ]
    assert c._test_take_combo() is None


def test_open_order_shows_the_combo():
    c, w = connected()
    c.place_order(10, combo([leg(SPY, "BUY"), leg(QQQ, "SELL")]), order())
    c._test_set_combo_view(10, SMART_COMBO, "QQQ,SPY", [(SPY, 1, "BUY", "SMART"), (QQQ, 1, "SELL", "SMART")],
                           "756733|1,320227571|-1", [1.7976931348623157e308, 1.7976931348623157e308])
    c._test_push_order_update(10, 0, "PreSubmitted", 0, 1)
    c._test_dispatch_once()
    order_id, contract, o, status = w.open[-1]
    assert (order_id, status) == (10, "PreSubmitted")
    assert (contract.conId, contract.symbol, contract.secType, contract.localSymbol, contract.tradingClass) == (SMART_COMBO, "QQQ,SPY", "BAG", "QQQ,SPY", "COMB")
    assert contract.comboLegsDescrip == "756733|1,320227571|-1"
    assert [(x.conId, x.ratio, x.action, x.exchange) for x in contract.comboLegs] == [(SPY, 1, "BUY", "SMART"), (QQQ, 1, "SELL", "SMART")]
    assert [x.price for x in o.orderComboLegs] == [1.7976931348623157e308] * 2
    assert [(t.tag, t.value) for t in o.smartComboRoutingParams] == [("NonGuaranteed", "1")]
