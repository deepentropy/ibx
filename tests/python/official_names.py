"""ibx#489: the official client library's names over the ibx Python client.

A scenario script written for the official client library (``placeOrder``,
``orderStatus``, ``connect(host, port, clientId)``) runs on ibx through this
module: ``EClient`` and ``EWrapper`` here carry the official names and hand
every call to the ibx classes of the same role.

    from official_names import EClient, EWrapper, Contract, Order

    class App(EWrapper, EClient):
        def __init__(self):
            EClient.__init__(self, self)

        def nextValidId(self, orderId):
            ...

    app = App()
    app.connect("127.0.0.1", 4002, 7)   # host and port are not used, see connect
    app.run()

What is converted, beyond the names:

- ``connect`` logs in with ``IB_USERNAME`` / ``IB_PASSWORD`` (and ``IB_HOST``
  when set) on the paper system, and stops unless the account id starts with
  ``DU``;
- ``error`` gets the official ``errorTime`` argument, which ibx does not have:
  the local clock in milliseconds;
- ``cancelOrder`` and ``reqGlobalCancel`` take the official ``OrderCancel``;
- ``exerciseOptions`` takes the three trailing arguments ibx does not have and
  refuses a value in any of them.

``NOT_IN_IBX`` lists the official names with no ibx counterpart, ``IBX_ONLY``
the ibx names with no official one; tests/python/test_official_names.py checks
both lists against the installed official client library.
"""

import inspect
import os
import re
import sys
import time

import ibx
from ibx import (BarData, ComboLeg, CommissionAndFeesReport, Contract, ContractDescription, ContractDetails,  # noqa: F401
                 DepthMktDataDescription, Execution, ExecutionCondition, HistoricalTick, HistoricalTickBidAsk,
                 HistoricalTickLast, MarginCondition, NewsProvider, Order, OrderAllocation, OrderComboLeg, OrderState,
                 PercentChangeCondition, PriceCondition, SmartComponent, SoftDollarTier, TagValue, TickAttrib,
                 TickAttribBidAsk, TickAttribLast, TickTypeEnum, TimeCondition, VolumeCondition, WshEventData)

UNSET_INTEGER = 2 ** 31 - 1
UNSET_DOUBLE = sys.float_info.max

# ── Names ──

# ibx names whose official form is not the plain camel case of their parts.
_OFFICIAL = {
    "req_pnl": "reqPnL",
    "cancel_pnl": "cancelPnL",
    "req_pnl_single": "reqPnLSingle",
    "cancel_pnl_single": "cancelPnLSingle",
    "request_fa": "requestFA",
    "replace_fa": "replaceFA",
    "receive_fa": "receiveFA",
    "replace_fa_end": "replaceFAEnd",
    "update_mkt_depth_l2": "updateMktDepthL2",
    "real_time_bar": "realtimeBar",
    "verify_message_api": "verifyMessageAPI",
    "verify_and_auth_message_api": "verifyAndAuthMessageAPI",
}

# Official keyword arguments whose ibx name is not their snake case (an ibx
# argument that is not used has the same name after an underscore).
_KEYWORDS = {
    "conid": "con_id",
    "ticker_id": "req_id",
    "fa_data": "fa_data_type",
    "impl_vol_options": "implied_vol_options",
}

# ibx requests and callbacks the official client library does not have.
IBX_ONLY = {
    "account_snapshot", "get_account_id", "last_rtt_ms", "next_order_id", "quote", "quote_by_instrument", "req_ping",
    "req_fundamental_data", "cancel_fundamental_data", "req_historical_schedule", "fundamental_data",
}

# Official requests and callbacks ibx does not have.
NOT_IN_IBX = {
    # connection set-up of a socket client
    "setConnState", "setConnectOptions", "setOptionalCapabilities", "startApi",
    # callbacks
    "rerouteMktDataReq", "rerouteMktDepthReq", "tickEFP", "winError",
}


def official_name(name):
    """The official name of an ibx request or callback."""
    if name in _OFFICIAL:
        return _OFFICIAL[name]
    head, *rest = name.split("_")
    return head + "".join(part.capitalize() for part in rest)


def ibx_keyword(name, parameters):
    """The ibx name of an official keyword argument, among the parameters of
    the ibx request."""
    snake = re.sub(r"(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z])", "_", name).lower()
    snake = _KEYWORDS.get(snake, snake)
    return snake if snake in parameters or "_" + snake not in parameters else "_" + snake


def _public(cls):
    return [n for n in dir(cls) if not n.startswith("_") and callable(getattr(cls, n))]


CALLBACKS = [n for n in _public(ibx.EWrapper) if n not in IBX_ONLY]
REQUESTS = [n for n in _public(ibx.EClient) if n not in IBX_ONLY]

# ── Objects of the official client library that ibx reads by attribute ──


class OrderCancel:
    def __init__(self):
        self.manualOrderCancelTime = ""
        self.extOperator = ""
        self.manualOrderIndicator = UNSET_INTEGER


class ExecutionFilter:
    def __init__(self):
        self.clientId = 0
        self.acctCode = ""
        self.time = ""
        self.symbol = ""
        self.secType = ""
        self.exchange = ""
        self.side = ""
        self.lastNDays = UNSET_INTEGER
        self.specificDates = None


class ScannerSubscription:
    def __init__(self):
        self.numberOfRows = -1
        self.instrument = ""
        self.locationCode = ""
        self.scanCode = ""
        self.abovePrice = UNSET_DOUBLE
        self.belowPrice = UNSET_DOUBLE
        self.aboveVolume = UNSET_INTEGER
        self.marketCapAbove = UNSET_DOUBLE
        self.marketCapBelow = UNSET_DOUBLE
        self.moodyRatingAbove = ""
        self.moodyRatingBelow = ""
        self.spRatingAbove = ""
        self.spRatingBelow = ""
        self.maturityDateAbove = ""
        self.maturityDateBelow = ""
        self.couponRateAbove = UNSET_DOUBLE
        self.couponRateBelow = UNSET_DOUBLE
        self.excludeConvertible = False
        self.averageOptionVolumeAbove = UNSET_INTEGER
        self.scannerSettingPairs = ""
        self.stockTypeFilter = ""


# ── Callbacks ──


def _error_args(args):
    req_id, code, text = args[:3]
    advanced = args[3] if len(args) > 3 else ""
    return req_id, int(time.time() * 1000), code, text, advanced


# Callbacks whose official arguments differ from the ibx ones.
_CALLBACK_ARGS = {"error": _error_args}


class EWrapper:
    """The official callbacks, each doing nothing until a subclass defines it."""


def _nothing(self, *args):
    pass


for _name in CALLBACKS:
    setattr(EWrapper, official_name(_name), _nothing)


class _Bridge(ibx.EWrapper):
    """The wrapper ibx calls: every callback goes to the official name of the
    target, looked up at the call so that a method set on the instance later
    is the one called."""

    def __init__(self, target):
        super().__init__()
        self._target = target


def _forward(name):
    official, convert = official_name(name), _CALLBACK_ARGS.get(name)

    def call(self, *args):
        getattr(self._target, official)(*(convert(args) if convert else args))

    call.__name__ = name
    return call


for _name in CALLBACKS:
    setattr(_Bridge, _name, _forward(_name))

# ── Requests ──


def assert_paper_account(account_id):
    """Stops unless the account is a paper account (its id starts with DU)."""
    if not account_id.startswith("DU"):
        raise SystemExit("refusing to go on: the logged-in account is not a paper account (its id does not start with DU)")


class EClient:
    """The official requests, made on an ibx client kept in ``self.ibx``."""

    def __init__(self, wrapper):
        self.wrapper = wrapper
        self.ibx = ibx.EClient(_Bridge(wrapper))

    def connect(self, host="127.0.0.1", port=4002, clientId=0):
        """Logs in to the paper system with the credentials of the environment.
        ``host`` and ``port`` name the reference's socket and are not used."""
        username, password = os.environ.get("IB_USERNAME"), os.environ.get("IB_PASSWORD")
        if not username or not password:
            raise RuntimeError("IB_USERNAME / IB_PASSWORD of the paper account are not set")
        to = {"host": os.environ["IB_HOST"]} if os.environ.get("IB_HOST") else {}
        self.ibx.connect(client_id=clientId, username=username, password=password, paper=True, **to)
        try:
            assert_paper_account(self.ibx.get_account_id())
        except SystemExit:
            self.ibx.disconnect()
            raise

    def cancelOrder(self, orderId, orderCancel=None):
        return self.ibx.cancel_order(orderId, getattr(orderCancel, "manualOrderCancelTime", ""))

    def reqGlobalCancel(self, orderCancel=None):
        return self.ibx.req_global_cancel()

    def exerciseOptions(self, reqId, contract, exerciseAction, exerciseQuantity, account, override, manualOrderTime="",
                        customerAccount="", professionalCustomer=False):
        if manualOrderTime or customerAccount or professionalCustomer:
            raise NotImplementedError("exerciseOptions: manualOrderTime, customerAccount and professionalCustomer "
                                      "have no ibx counterpart")
        return self.ibx.exercise_options(reqId, contract, exerciseAction, exerciseQuantity, account, override)


def ibx_parameters(name):
    """The parameter names of an ibx request."""
    return [p for p in inspect.signature(getattr(ibx.EClient, name)).parameters if p != "self"]


def _request(name):
    parameters = ibx_parameters(name)

    def call(self, *args, **kwargs):
        return getattr(self.ibx, name)(*args, **{ibx_keyword(k, parameters): v for k, v in kwargs.items()})

    call.__name__ = official_name(name)
    call.__doc__ = f"``{name}`` of the ibx client."
    return call


for _name in REQUESTS:
    if official_name(_name) not in vars(EClient):
        setattr(EClient, official_name(_name), _request(_name))
