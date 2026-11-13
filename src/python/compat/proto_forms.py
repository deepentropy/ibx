"""The protobuf form of the requests and callbacks of the Python client (ibx#725).

The official client library gives each request a second method whose name ends in
``ProtoBuf`` and which takes one protobuf message, and calls for each answer a callback
whose name ends in ``ProtoBuf`` with the answer's protobuf message, before the plain
callback. This module gives both to the ibx client:

- every ``..._proto_buf`` request reads its message (the official class, or any object
  with the same fields) and makes the plain request of the client;
- a wrapper that defines ``..._proto_buf`` callbacks gets each of them before the plain
  callback, with the official protobuf class when the official library is installed,
  otherwise with a ``ProtoForm`` (the same attributes; a field that is not set is None,
  a list is empty).

A wrapper that defines none of them is called directly: nothing of this module is on
its path.
"""
import enum
import inspect
import sys
import time
from decimal import Decimal

UNSET_INTEGER = 2 ** 31 - 1
UNSET_LONG = 2 ** 63 - 1
UNSET_DOUBLE = sys.float_info.max
UNSET_DECIMAL = Decimal(2 ** 127 - 1)
NO_VALID_ID = -1

T = None        # the tables of proto_table.py
X = None        # the ibx module
KINDS = {}      # message -> {field: kind}


# ── Protobuf messages ──

_classes = {}


def official(message):
    """The official class of a protobuf message; None when the official library is not installed."""
    if message not in _classes:
        try:
            _classes[message] = getattr(__import__(f"ibapi.protobuf.{message}_pb2", fromlist=[message]), message)
        except Exception:  # noqa: BLE001  (not installed, or installed without its protobuf package)
            _classes[message] = None
    return _classes[message]


class ProtoForm:
    """A protobuf message when the official library is not installed: the fields of the
    message as attributes, None for a field that is not set, an empty list or dict for a
    repeated one."""

    def __init__(self, message, fields):
        self._message = message
        for name, kind in T.MESSAGES[message]:
            v = fields.get(name)
            if kind == "map":
                v = dict(v or {})
            elif kind[0] == "*":
                v = [ProtoForm(kind[3:], x) for x in v or ()] if kind[1] == "m" else [_as(x, kind[1]) for x in v or ()]
            elif kind[0] == "m":
                v = ProtoForm(kind[2:], v) if v is not None else None
            elif v is not None:
                v = _as(v, kind)
            setattr(self, name, v)

    def HasField(self, name):
        return getattr(self, name) is not None

    def __repr__(self):
        given = {n: getattr(self, n) for n, _ in T.MESSAGES[self._message] if getattr(self, n) not in (None, [], {})}
        return f"{self._message}({given})"


def _plain_value(v):
    if isinstance(v, enum.Enum):
        v = v.value
    return v[0] if isinstance(v, tuple) else v


def _text(v):
    """A number as the text of a protobuf text field."""
    if isinstance(v, float):
        return repr(v)
    return str(v)


def _as(v, kind):
    """A value as the type of a protobuf field."""
    v = _plain_value(v)
    if kind == "s":
        return v if isinstance(v, str) else _text(v)
    if kind == "i":
        return int(v)
    if kind == "f":
        return float(v)
    if kind == "b":
        return bool(v)
    return v


def _fill(m, message, fields):
    kinds = KINDS[message]
    for name, v in fields.items():
        kind = kinds[name]
        if kind == "map":
            getattr(m, name).update(v)
        elif kind[0] == "*":
            if kind[1] == "m":
                for x in v:
                    _fill(getattr(m, name).add(), kind[3:], x)
            else:
                getattr(m, name).extend(_as(x, kind[1]) for x in v)
        elif kind[0] == "m":
            sub = getattr(m, name)
            sub.SetInParent()
            _fill(sub, kind[2:], v)
        else:
            setattr(m, name, _as(v, kind))


def make(message, fields):
    """The protobuf message `message` with `fields` set: {field: value}, a message as a
    dict of its own fields, a repeated field as a list."""
    cls = official(message)
    if cls is None:
        return ProtoForm(message, fields)
    m = cls()
    _fill(m, message, fields)
    return m


def _given(v, kind):
    """Whether a plain value is one a protobuf field is set for: not the value that
    stands for none."""
    if v is None:
        return False
    if kind == "i":
        return v != UNSET_INTEGER and v != UNSET_LONG
    if kind == "f":
        return v != UNSET_DOUBLE and v != ""
    if kind == "s":
        return v != "" and v != UNSET_DECIMAL and v != UNSET_DOUBLE
    return True


# Which fields a message has set, as the reference sets them (10.51, paper, 13/11/2026,
# regular hours, 62 kinds of callbacks):
# - a field of an object (a contract, an order, an execution, contract details...) is left
#   out when its value is the one the object starts with, which is the value the official
#   decoder gives the callback without the field: a client reads the same either way.
#   The fields below are the ones the reference sets at that value all the same;
# - a number given directly by a callback is set, 0 too; a text is set when not empty,
#   a yes/no when yes.
_SET_AT_START = frozenset({
    ("Order", "adjustableTrailingUnit"), ("Order", "clientId"), ("Order", "deltaNeutralShortSaleSlot"), ("Order", "exemptCode"),
    ("Order", "orderId"), ("Order", "origin"), ("Order", "parentId"), ("Order", "shortSaleSlot"), ("Order", "triggerMethod"),
    ("Execution", "clientId"), ("Execution", "optExerciseOrLapseType"),
})
_NO_START = object()


def _put(d, kinds, field, v, start=_NO_START, message=None):
    """Sets `field` to `v` unless `v` stands for none, or is `start`: the value the
    official decoder gives the callback when the field is not set."""
    if not _given(v, kinds[field]):
        return
    if start is not _NO_START and v == start and (message, field) not in _SET_AT_START:
        return
    d[field] = v


_fresh_objects = {}


def _fresh(cls):
    """An object of `cls` as it starts: the values the official decoder leaves when a
    field is not set."""
    if cls not in _fresh_objects:
        try:
            _fresh_objects[cls] = cls()
        except Exception:  # noqa: BLE001  (a class that cannot be made without values)
            _fresh_objects[cls] = None
    return _fresh_objects[cls]


# ── Requests: a protobuf message read back into the plain request ──

def _has(o, name):
    has = getattr(o, "HasField", None)
    if has is None:
        return getattr(o, name, None) is not None
    try:
        return has(name)
    except ValueError:      # a field with no presence (a list, or a plain scalar)
        return bool(getattr(o, name))


_UNSET = {"i": UNSET_INTEGER, "f": UNSET_DOUBLE, "s": "", "b": False}


def _scalar(o, name, kind):
    if o is not None and _has(o, name):
        return getattr(o, name)
    if name == "reqId":
        return NO_VALID_ID
    return 0 if name == "orderId" else _UNSET[kind]


def _float_max_string(v):
    return "" if v == UNSET_DOUBLE else f"{v:.8f}".rstrip("0").rstrip(".").rstrip(",")


def _back(v, conv):
    """A protobuf value as the plain value the official creator was given."""
    if conv == "float":                 # a text of the plain object sent as a number
        return _float_max_string(v)
    if conv == "decimalMaxString":
        return Decimal(v)
    if conv == "bool01":
        return bool(v)
    return v


def _copy(creator, variable, p, o):
    """Sets on `o` the attributes the official creator reads from its `variable`, for the
    fields set in `p`."""
    for var, attr, field, when, conv in T.CREATORS[creator]:
        if var != variable:
            continue
        if _has(p, field):
            setattr(o, attr, _back(getattr(p, field), conv))
        elif when == "truthy":
            # The official creator leaves such a field out for no, nothing or zero: a
            # value of the object that starts as yes (an order's `transmit`) is put to no.
            start = getattr(o, attr, None)
            if start and isinstance(start, (bool, int, float, str)):
                setattr(o, attr, type(start)())
    return o


def _tags(p, name):
    m = getattr(p, name, None) if p is not None else None
    return [X.TagValue(str(k), str(v)) for k, v in dict(m).items()] if m else []


class _Plain:
    """A request object the client reads by attribute, with the values the official
    library gives it."""

    def __init__(self, name):
        self.__dict__.update(T.DEFAULTS[name])


def _contract(p, order=None):
    c = _copy("createContractProto", "contract", p, X.Contract())
    legs, prices = [], []
    for leg in getattr(p, "comboLegs", None) or ():
        legs.append(_copy("createComboLegProto", "comboLeg", leg, X.ComboLeg()))
        price = X.OrderComboLeg()
        if _has(leg, "perLegPrice"):
            price.price = leg.perLegPrice
        prices.append(price)
    if legs:
        c.comboLegs = legs
        if order is not None and any(_has(leg, "perLegPrice") for leg in p.comboLegs):
            order.orderComboLegs = prices
    if _has(p, "deltaNeutralContract"):
        d = p.deltaNeutralContract
        c.deltaNeutralContract = _Namespace(conId=_scalar(d, "conId", "i"), delta=_scalar(d, "delta", "f"), price=_scalar(d, "price", "f"))
    return c


class _Namespace:
    def __init__(self, **fields):
        self.__dict__.update(fields)


# Condition type -> (the official creator of its own fields, its variable there, whether it
# is on a contract; None: it has no "is more").
_CONDITIONS = {1: ("createPriceConditionProto", "priceCondition", True),
               3: ("createTimeConditionProto", "timeCondition", False),
               4: ("createMarginConditionProto", "marginCondition", False),
               5: ("createExecutionConditionProto", "executionCondition", None),
               6: ("createVolumeConditionProto", "volumeCondition", True),
               7: ("createPercentChangeConditionProto", "percentChangeCondition", True)}
_CONDITION_VALUES = ("conId", "exchange", "price", "triggerMethod", "time", "percent", "secType", "symbol", "volume", "changePercent")


def _condition(p):
    """An order condition as the client reads one: by its attributes. It keeps how it is
    joined to the next one, which the client's own condition classes do not hold."""
    kind = _scalar(p, "type", "i")
    if kind not in _CONDITIONS:
        raise ValueError(f"order condition of type {kind!r}: not a type of the official library")
    creator, variable, on_contract = _CONDITIONS[kind]
    c = _Namespace(condType=kind, isConjunctionConnection=True, **dict.fromkeys(_CONDITION_VALUES))
    if _has(p, "isConjunctionConnection"):
        c.isConjunctionConnection = bool(p.isConjunctionConnection)
    if on_contract is not None:
        c.isMore = bool(p.isMore) if _has(p, "isMore") else False
    if on_contract:
        _copy("createContractConditionProto", "contractCondition", p, c)
    return _copy(creator, variable, p, c)


def _order(p, attached=None):
    o = _copy("createOrderProto", "order", p, X.Order())
    for name in ("algoParams", "smartComboRoutingParams", "orderMiscOptions"):
        tags = _tags(p, name)
        if tags:
            setattr(o, name, tags)
    conditions = [_condition(c) for c in getattr(p, "conditions", None) or ()]
    if conditions:
        o.conditions = conditions
    if _has(p, "softDollarTier"):
        t = p.softDollarTier
        o.softDollarTier = X.SoftDollarTier(_scalar(t, "name", "s"), _scalar(t, "value", "s"), _scalar(t, "displayName", "s"))
    if attached is not None:
        _copy("createAttachedOrdersProto", "order", attached, o)
    return o


def _sub(p, name):
    return getattr(p, name) if p is not None and _has(p, name) else None


def _place_order(client, p):
    order = _order(_sub(p, "order"), _sub(p, "attachedOrders")) if _has(p, "order") else X.Order()
    contract = _contract(p.contract, order) if _has(p, "contract") else X.Contract()
    return client.place_order(_scalar(p, "orderId", "i"), contract, order)


def _cancel_order(client, p):
    cancel = _sub(p, "orderCancel")
    return client.cancel_order(_scalar(p, "orderId", "i"), _scalar(cancel, "manualOrderCancelTime", "s"))


def _global_cancel(client, p):
    return client.req_global_cancel()


def _executions(client, p):
    f, given = _Plain("ExecutionFilter"), _sub(p, "executionFilter")
    if given is not None:
        _copy("createExecutionRequestProto", "execFilter", given, f)
        if list(getattr(given, "specificDates", None) or ()):
            f.specificDates = list(given.specificDates)
    return client.req_executions(_scalar(p, "reqId", "i"), f)


def _scanner_subscription(client, p):
    s, given = _Plain("ScannerSubscription"), _sub(p, "scannerSubscription")
    if given is not None:
        _copy("createScannerSubscriptionProto", "subscription", given, s)
    return client.req_scanner_subscription(_scalar(p, "reqId", "i"), s, _tags(given, "scannerSubscriptionOptions"),
                                           _tags(given, "scannerSubscriptionFilterOptions"))


def _wsh_event_data(client, p):
    return client.req_wsh_event_data(_scalar(p, "reqId", "i"), _copy("createWshEventDataRequestProto", "wshEventData", p, X.WshEventData()))


def _exercise_options(client, p):
    # The three fields the plain request of the client does not have are refused, not dropped.
    for name in ("manualOrderTime", "customerAccount", "professionalCustomer"):
        if _has(p, name) and getattr(p, name):
            raise NotImplementedError(f"exercise_options_proto_buf: {name} has no counterpart in the client")
    contract = _contract(p.contract) if _has(p, "contract") else X.Contract()
    return client.exercise_options(_scalar(p, "orderId", "i"), contract, _scalar(p, "exerciseAction", "i"),
                                   _scalar(p, "exerciseQuantity", "i"), _scalar(p, "account", "s"), int(_scalar(p, "override", "b")))


# Protobuf request (its name without _proto_buf) -> the plain request and the fields of the
# message in the order of the plain request's arguments (":int" for a yes/no the plain
# request takes as a number), or the function that makes the plain request.
_REQUESTS = {
    "req_market_data": ("req_mkt_data", "reqId contract genericTickList snapshot regulatorySnapshot marketDataOptions"),
    "cancel_market_data": ("cancel_mkt_data", "reqId"),
    "place_order": _place_order,
    "cancel_order": _cancel_order,
    "req_open_orders": ("req_open_orders", ""),
    "req_account_updates": ("req_account_updates", "subscribe acctCode"),
    "req_executions": _executions,
    "req_ids": ("req_ids", "numIds"),
    "req_contract_data": ("req_contract_details", "reqId contract"),
    "req_market_depth": ("req_mkt_depth", "reqId contract numRows isSmartDepth marketDepthOptions"),
    "cancel_market_depth": ("cancel_mkt_depth", "reqId isSmartDepth"),
    "req_news_bulletins": ("req_news_bulletins", "allMessages"),
    "cancel_news_bulletins": ("cancel_news_bulletins", ""),
    "set_server_log_level": ("set_server_log_level", "logLevel"),
    "req_auto_open_orders": ("req_auto_open_orders", "autoBind"),
    "req_all_open_orders": ("req_all_open_orders", ""),
    "req_managed_accts": ("req_managed_accts", ""),
    "req_fa": ("request_fa", "faDataType"),
    "replace_fa": ("replace_fa", "reqId faDataType xml"),
    "req_historical_data": ("req_historical_data", "reqId contract endDateTime duration barSizeSetting whatToShow useRTH:int formatDate "
                                                   "keepUpToDate chartOptions"),
    "exercise_options": _exercise_options,
    "req_scanner_subscription": _scanner_subscription,
    "cancel_scanner_subscription": ("cancel_scanner_subscription", "reqId"),
    "req_scanner_parameters": ("req_scanner_parameters", ""),
    "cancel_historical_data": ("cancel_historical_data", "reqId"),
    "req_current_time": ("req_current_time", ""),
    "req_real_time_bars": ("req_real_time_bars", "reqId contract barSize whatToShow useRTH:int realTimeBarsOptions"),
    "cancel_real_time_bars": ("cancel_real_time_bars", "reqId"),
    "calculate_implied_volatility": ("calculate_implied_volatility", "reqId contract optionPrice underPrice impliedVolatilityOptions"),
    "calculate_option_price": ("calculate_option_price", "reqId contract volatility underPrice optionPriceOptions"),
    "cancel_calculate_implied_volatility": ("cancel_calculate_implied_volatility", "reqId"),
    "cancel_calculate_option_price": ("cancel_calculate_option_price", "reqId"),
    "req_global_cancel": _global_cancel,
    "req_market_data_type": ("req_market_data_type", "marketDataType"),
    "req_positions": ("req_positions", ""),
    "req_account_summary": ("req_account_summary", "reqId group tags"),
    "cancel_account_summary": ("cancel_account_summary", "reqId"),
    "cancel_positions": ("cancel_positions", ""),
    "verify_request": ("verify_request", "apiName apiVersion"),
    "verify_message": ("verify_message", "apiData"),
    "query_display_groups": ("query_display_groups", "reqId"),
    "subscribe_to_group_events": ("subscribe_to_group_events", "reqId groupId"),
    "update_display_group": ("update_display_group", "reqId contractInfo"),
    "unsubscribe_from_group_events": ("unsubscribe_from_group_events", "reqId"),
    "req_positions_multi": ("req_positions_multi", "reqId account modelCode"),
    "cancel_positions_multi": ("cancel_positions_multi", "reqId"),
    "req_account_updates_multi": ("req_account_updates_multi", "reqId account modelCode ledgerAndNLV"),
    "cancel_account_updates_multi": ("cancel_account_updates_multi", "reqId"),
    "req_sec_def_opt_params": ("req_sec_def_opt_params", "reqId underlyingSymbol futFopExchange underlyingSecType underlyingConId"),
    "req_soft_dollar_tiers": ("req_soft_dollar_tiers", "reqId"),
    "req_family_codes": ("req_family_codes", ""),
    "req_matching_symbols": ("req_matching_symbols", "reqId pattern"),
    "req_market_depth_exchanges": ("req_mkt_depth_exchanges", ""),
    "req_smart_components": ("req_smart_components", "reqId bboExchange"),
    "req_news_article": ("req_news_article", "reqId providerCode articleId newsArticleOptions"),
    "req_news_providers": ("req_news_providers", ""),
    "req_historical_news": ("req_historical_news", "reqId conId providerCodes startDateTime endDateTime totalResults historicalNewsOptions"),
    "req_head_timestamp": ("req_head_time_stamp", "reqId contract whatToShow useRTH:int formatDate"),
    "req_histogram_data": ("req_histogram_data", "reqId contract useRTH timePeriod"),
    "cancel_histogram_data": ("cancel_histogram_data", "reqId"),
    "cancel_head_timestamp": ("cancel_head_time_stamp", "reqId"),
    "req_market_rule": ("req_market_rule", "marketRuleId"),
    "req_pnl": ("req_pnl", "reqId account modelCode"),
    "cancel_pnl": ("cancel_pnl", "reqId"),
    "req_pnl_single": ("req_pnl_single", "reqId account modelCode conId"),
    "cancel_pnl_single": ("cancel_pnl_single", "reqId"),
    "req_historical_ticks": ("req_historical_ticks", "reqId contract startDateTime endDateTime numberOfTicks whatToShow useRTH:int ignoreSize "
                                                     "miscOptions"),
    "req_tick_by_tick_data": ("req_tick_by_tick_data", "reqId contract tickType numberOfTicks ignoreSize"),
    "cancel_tick_by_tick": ("cancel_tick_by_tick_data", "reqId"),
    "req_completed_orders": ("req_completed_orders", "apiOnly"),
    "req_wsh_meta_data": ("req_wsh_meta_data", "reqId"),
    "cancel_wsh_meta_data": ("cancel_wsh_meta_data", "reqId"),
    "req_wsh_event_data": _wsh_event_data,
    "cancel_wsh_event_data": ("cancel_wsh_event_data", "reqId"),
    "req_user_info": ("req_user_info", "reqId"),
    "req_current_time_in_millis": ("req_current_time_in_millis", ""),
    "cancel_contract_data": ("cancel_contract_data", "reqId"),
    "cancel_historical_ticks": ("cancel_historical_ticks", "reqId"),
}

# Protobuf requests the client does not give: the start of the API session is part of connect().
NOT_GIVEN = ("start_api",)


def _request(name, message, spec, argument):
    """The method `<name>_proto_buf` of the client; its argument has the official name."""
    def given(args, kwargs):
        if len(args) + len(kwargs) != 1 or (kwargs and argument not in kwargs):
            raise TypeError(f"{name}_proto_buf() takes one argument, {argument}")
        return args[0] if args else kwargs[argument]

    if callable(spec):
        def call(self, *a, **k):
            proto = given(a, k)
            return None if proto is None else spec(self, proto)
    else:
        plain, kinds, plan = spec[0], KINDS[message], []
        for word in spec[1].split():
            field, _, conv = word.partition(":")
            plan.append((field, kinds[field], conv))

        def call(self, *a, **k):
            proto = given(a, k)
            if proto is None:
                return None
            args = []
            for field, kind, conv in plan:
                if kind == "m:Contract":
                    v = _contract(getattr(proto, field)) if _has(proto, field) else X.Contract()
                elif kind == "map":
                    v = _tags(proto, field)
                else:
                    v = _scalar(proto, field, kind)
                    if conv == "int":
                        v = int(v)
                args.append(v)
            return getattr(self, plain)(*args)
    call.__name__ = call.__qualname__ = name + "_proto_buf"
    call.__signature__ = inspect.Signature([inspect.Parameter(n, inspect.Parameter.POSITIONAL_OR_KEYWORD) for n in ("self", argument)])
    call.__doc__ = (f"The request of the message `{message}` (the official class, or an object with its fields), as the official "
                    f"`{T.OFFICIAL_NAMES[name + '_proto_buf']}` (ibx#725).")
    return call


def _use_proto_buf(self, msg_id):
    """Whether the official library sends the request of this outgoing message id in its
    protobuf form on this session, as the official `useProtoBuf` (ibx#725)."""
    level = T.REQUESTS[msg_id][2] if msg_id in T.REQUESTS else 0
    version = self.server_version()
    return bool(level) and version is not None and level <= version


def _send_msg_proto_buf(self, msg_id, msg):
    """The request of a serialized protobuf message, as the official `sendMsgProtoBuf`:
    `msg_id` is the outgoing message id plus 200, `msg` the bytes of the message. It is
    read with the official library, which has to be installed (ibx#725)."""
    entry = T.REQUESTS.get(msg_id - 200)
    if entry is None or not hasattr(self, entry[0]):
        raise ValueError(f"send_msg_proto_buf: {msg_id} is not the id of a protobuf request of the client")
    cls = official(entry[1])
    if cls is None:
        raise RuntimeError("send_msg_proto_buf: the official library is needed to read a serialized message")
    proto = cls()
    proto.ParseFromString(bytes(msg))
    return getattr(self, entry[0])(proto)


# ── Callbacks: the protobuf message of a plain callback ──

def _rows(decoder, message_variable, variable, o, message, d=None):
    """The fields the official decoder reads into the attributes of its `variable`, from
    the plain object `o`."""
    d = {} if d is None else d
    kinds, fresh = KINDS[message], _fresh(type(o))
    for mv, field, var, attr, conv in T.OBJECTS[decoder]:
        if mv == message_variable and var == variable:
            v = getattr(o, attr, None)
            if conv == "bool01" and not v:
                continue
            start = _plain_value(getattr(fresh, attr, _NO_START)) if fresh is not None else _NO_START
            _put(d, kinds, field, _plain_value(v), start, message)
    return d


def _tag_map(tags):
    return {str(t.tag): str(t.value) for t in tags or ()}


def _contract_fields(c, order=None):
    d = _rows("decodeContract", "contractProto", "contract", c, "Contract")
    legs = []
    prices = list(getattr(order, "orderComboLegs", None) or ()) if order is not None else []
    for i, leg in enumerate(getattr(c, "comboLegs", None) or ()):
        f = _rows("decodeComboLegs", "comboLegProto", "comboLeg", leg, "ComboLeg")
        if i < len(prices):
            _put(f, KINDS["ComboLeg"], "perLegPrice", prices[i].price)
        legs.append(f)
    if legs:
        d["comboLegs"] = legs
    dn = getattr(c, "deltaNeutralContract", None)
    if dn is not None:
        d["deltaNeutralContract"] = _rows("decodeDeltaNeutralContract", "deltaNeutralContractProto", "deltaNeutralContract", dn,
                                          "DeltaNeutralContract")
    return d


_CONDITION_FIELDS = (("isMore", "isMore"), ("conId", "conId"), ("exchange", "exchange"), ("symbol", "symbol"), ("secType", "secType"),
                     ("percent", "percent"), ("changePercent", "changePercent"), ("price", "price"), ("triggerMethod", "triggerMethod"),
                     ("time", "time"), ("volume", "volume"))


def _condition_fields(c):
    kinds = KINDS["OrderCondition"]
    d = {"type": int(_plain_value(c.condType)), "isConjunctionConnection": bool(c.isConjunctionConnection)}
    for field, attr in _CONDITION_FIELDS:
        v = getattr(c, attr, None)
        if v is not None and not callable(v):
            if kinds[field] == "b":
                d[field] = bool(v)
            else:
                _put(d, kinds, field, v)
    return d


def _order_fields(o):
    d = _rows("decodeOrder", "orderProto", "order", o, "Order")
    routing = _tag_map(getattr(o, "smartComboRoutingParams", None))
    if routing:
        d["smartComboRoutingParams"] = routing
    if getattr(o, "hedgeType", "") and getattr(o, "hedgeParam", ""):
        d["hedgeParam"] = o.hedgeParam
    if getattr(o, "algoStrategy", ""):
        d["algoStrategy"] = o.algoStrategy
        params = _tag_map(getattr(o, "algoParams", None))
        if params:
            d["algoParams"] = params
    conditions = [_condition_fields(c) for c in getattr(o, "conditions", None) or ()]
    if conditions:
        d["conditions"] = conditions
    tier = getattr(o, "softDollarTier", None)
    if tier is not None:
        f = {}
        for field, attr in (("name", "name"), ("value", "val"), ("displayName", "displayName")):
            _put(f, KINDS["SoftDollarTier"], field, getattr(tier, attr, None))
        if f:
            d["softDollarTier"] = f
    return d


def _order_state_fields(s):
    d = _rows("decodeOrderState", "orderStateProto", "orderState", s, "OrderState")
    allocations = [_rows("decodeOrderAllocations", "orderAllocationProto", "orderAllocation", a, "OrderAllocation")
                   for a in getattr(s, "orderAllocations", None) or ()]
    if allocations:
        d["orderAllocations"] = allocations
    return d


def _details_fields(cd, bond):
    """The two messages of a contract's details: its contract and its details."""
    contract = _contract_fields(cd.contract)
    # The official decoder splits the last trade date of the contract into a date, a time
    # and, for a bond, a time zone: they are put back together here.
    parts = [cd.maturity if bond else getattr(cd.contract, "lastTradeDateOrContractMonth", ""), getattr(cd, "lastTradeTime", "")]
    # The time comes with its time zone (the reference 10.51, 13/11/2026, an AAPL
    # option: "20261120 16:15:00 US/Eastern").
    if parts[1]:
        parts.append(getattr(cd, "timeZoneId", ""))
    while parts and not parts[-1]:
        parts.pop()
    if parts and parts[0]:
        contract["lastTradeDateOrContractMonth"] = " ".join(parts)
    d = _rows("decodeContractDetails", "contractDetailsProto", "contractDetails", cd, "ContractDetails")
    ids = _tag_map(getattr(cd, "secIdList", None))
    if ids:
        d["secIdList"] = ids
    reasons = [_rows("decodeIneligibilityReasonList", "ineligibilityReasonProto", "ineligibilityReason", r, "IneligibilityReason")
               for r in getattr(cd, "ineligibilityReasonList", None) or ()]
    if reasons:
        d["ineligibilityReasonList"] = reasons
    return contract, d


def _bar_fields(bar):
    return _rows("decodeHistoricalDataBar", "historicalDataBarProto", "bar", bar, "HistoricalDataBar")


def _flags(o, names):
    """The attributes of a tick: the reference sets each of them, yes or no."""
    return {n: bool(getattr(o, n, False)) for n in names}


def _tick_last_fields(t):
    d = _rows("decodeHistoricalTickLast", "historicalTickLastProto", "historicalTickLast", t, "HistoricalTickLast")
    d["tickAttribLast"] = _flags(getattr(t, "tickAttribLast", None), ("pastLimit", "unreported"))
    return d


def _tick_bid_ask_fields(t):
    d = _rows("decodeHistoricalTickBidAsk", "historicalTickBidAskProto", "historicalTickBidAsk", t, "HistoricalTickBidAsk")
    d["tickAttribBidAsk"] = _flags(getattr(t, "tickAttribBidAsk", None), ("bidPastLow", "askPastHigh"))
    return d


def _tick_fields(t):
    return _rows("decodeHistoricalTick", "historicalTickProto", "historicalTick", t, "HistoricalTick")


def _part(x, index, attr):
    """A value of an entry the client gives as a tuple, or as an object of the official library."""
    return x[index] if isinstance(x, (tuple, list)) else getattr(x, attr)


def _head(name, args, d=None):
    """The fields of the arguments the table gives a field for."""
    d = {} if d is None else d
    message = T.ANSWERS[name][1]
    kinds = KINDS[message]
    for v, spec in zip(args, T.ANSWERS[name][2]):
        if spec is not None:
            _put(d, kinds, spec[0], v, False if kinds[spec[0]] == "b" else _NO_START, message)
    return d


def _error(proxy, args):
    req_id, code, text = args[:3]
    if code in _CLIENT_ERRORS:
        return None
    d = {"id": req_id, "errorTime": int(time.time() * 1000), "errorCode": code, "errorMsg": text}
    if len(args) > 3 and args[3]:
        d["advancedOrderRejectJson"] = args[3]
    return d


def _tick_price(proxy, args):
    req_id, tick_type, price, attrib = args
    d = _head("tick_price", args)
    size, proxy._size = proxy._size, None
    if size is not None:
        d["size"] = size
        proxy._size_given = (req_id, tick_type)
    mask = (1 if attrib.canAutoExecute else 0) | (2 if attrib.pastLimit else 0) | (4 if attrib.preOpen else 0)
    d["attrMask"] = mask
    return d


def _tick_req_params(proxy, args):
    d = _head("tick_req_params", args)
    precisions, proxy._precisions = proxy._precisions, None
    if precisions is not None:
        for field, text in zip(("lastPricePrecision", "lastSizePrecision"), precisions):
            if text:
                d[field] = text
    return d


def _tick_size(proxy, args):
    # The size of a price message is part of that message: no message of its own.
    given, proxy._size_given = proxy._size_given, None
    if given is not None and given[0] == args[0]:
        return None
    return _head("tick_size", args)


def _open_order(proxy, args):
    order_id, contract, order, state = args
    return {"orderId": order_id, "contract": _contract_fields(contract, order), "order": _order_fields(order),
            "orderState": _order_state_fields(state)}


def _completed_order(proxy, args):
    contract, order, state = args
    return {"contract": _contract_fields(contract, order), "order": _order_fields(order), "orderState": _order_state_fields(state)}


def _with_contract(name, index):
    def build(proxy, args):
        d = _head(name, args)
        d["contract"] = _contract_fields(args[index])
        return d
    return build


def _contract_details(bond):
    def build(proxy, args):
        contract, details = _details_fields(args[1], bond)
        return {"reqId": args[0], "contract": contract, "contractDetails": details}
    return build


def _exec_details(proxy, args):
    return {"reqId": args[0], "contract": _contract_fields(args[1]),
            "execution": _rows("decodeExecution", "executionProto", "execution", args[2], "Execution")}


def _commission_report(proxy, args):
    r, d, kinds = args[0], {}, KINDS["CommissionAndFeesReport"]
    for field, attr, start in (("execId", "execId", ""), ("commissionAndFees", "commissionAndFees", 0.0), ("currency", "currency", ""),
                               ("realizedPNL", "realizedPNL", 0.0), ("bondYield", "yield_", 0.0),
                               ("yieldRedemptionDate", "yieldRedemptionDate", 0)):
        _put(d, kinds, field, getattr(r, attr, None), start)
    return d


def _historical_data_update(proxy, args):
    return {"reqId": args[0], "historicalDataBar": _bar_fields(args[1])}


def _sec_def_opt_parameter(proxy, args):
    d = _head("security_definition_option_parameter", args)
    for field, values in (("expirations", args[5]), ("strikes", args[6])):
        values = sorted(values) if isinstance(values, (set, frozenset)) else list(values or ())
        if values:
            d[field] = values
    return d


def _soft_dollar_tiers(proxy, args):
    tiers = []
    for t in args[1] or ():
        f = {}
        for field, attr in (("name", "name"), ("value", "val"), ("displayName", "displayName")):
            _put(f, KINDS["SoftDollarTier"], field, getattr(t, attr, None))
        tiers.append(f)
    return {"reqId": args[0], "softDollarTiers": tiers}


def _family_codes(proxy, args):
    return {"familyCodes": [_named("FamilyCode", accountId=_part(c, 0, "accountID"), familyCode=_part(c, 1, "familyCodeStr"))
                            for c in args[0] or ()]}


def _named(message, **values):
    d, kinds = {}, KINDS[message]
    for field, v in values.items():
        _put(d, kinds, field, v)
    return d


def _symbol_samples(proxy, args):
    out = []
    for cd in args[1] or ():
        f = {"contract": _contract_fields(cd.contract)}
        types = list(getattr(cd, "derivativeSecTypes", None) or ())
        if types:
            f["derivativeSecTypes"] = types
        out.append(f)
    return {"reqId": args[0], "contractDescriptions": out}


def _smart_components(proxy, args):
    components = []
    for bit, v in dict(args[1] or {}).items():
        f = _named("SmartComponent", exchange=v[0], exchangeLetter=v[1])
        if bit:                 # the reference leaves bit 0 out
            f["bitNumber"] = bit
        components.append(f)
    return {"reqId": args[0], "smartComponents": components}


def _depth_exchanges(proxy, args):
    return {"depthMarketDataDescriptions": [
        _rows("decodeDepthMarketDataDescription", "depthMarketDataDescriptionProto", "description", x, "DepthMarketDataDescription")
        for x in args[0] or ()]}


def _news_providers(proxy, args):
    return {"newsProviders": [_named("NewsProvider", providerCode=p.code, providerName=p.name) for p in args[0] or ()]}


def _histogram(proxy, args):
    return {"reqId": args[0], "histogramDataEntries": [_named("HistogramDataEntry", price=_part(e, 0, "price"), size=_part(e, 1, "size"))
                                                       for e in args[1] or ()]}


def _market_rule(proxy, args):
    return {"marketRuleId": args[0], "priceIncrements": [_named("PriceIncrement", lowEdge=_part(i, 0, "lowEdge"),
                                                                increment=_part(i, 1, "increment")) for i in args[1] or ()]}


def _ticks(field, one):
    def build(proxy, args):
        d = {"reqId": args[0], field: [one(t) for t in args[1] or ()]}
        if args[2]:
            d["isDone"] = True
        return d
    return build


def _tick_by_tick_last(proxy, args):
    req_id, tick_type, when, price, size, attrib, exchange, conditions = args
    last = _named("HistoricalTickLast", time=when, price=price, size=size, exchange=exchange, specialConditions=conditions)
    last["tickAttribLast"] = _flags(attrib, ("pastLimit", "unreported"))
    return {"reqId": req_id, "tickType": tick_type, "historicalTickLast": last}


def _tick_by_tick_bid_ask(proxy, args):
    req_id, when, bid, ask, bid_size, ask_size, attrib = args
    tick = _named("HistoricalTickBidAsk", time=when, priceBid=bid, priceAsk=ask, sizeBid=bid_size, sizeAsk=ask_size)
    tick["tickAttribBidAsk"] = _flags(attrib, ("bidPastLow", "askPastHigh"))
    return {"reqId": req_id, "tickType": 3, "historicalTickBidAsk": tick}


def _tick_by_tick_mid(proxy, args):
    return {"reqId": args[0], "tickType": 4, "historicalTickMidPoint": _named("HistoricalTick", time=args[1], price=args[2])}


def _depth(proxy, args):
    req_id, position, operation, side, price, size = args
    return {"reqId": req_id, "marketDepthData": _named("MarketDepthData", position=position, operation=operation, side=side, price=price,
                                                       size=size)}


def _depth_l2(proxy, args):
    req_id, position, maker, operation, side, price, size, smart = args
    data = _named("MarketDepthData", position=position, marketMaker=maker, operation=operation, side=side, price=price, size=size)
    if smart:
        data["isSmartDepth"] = True
    return {"reqId": req_id, "marketDepthData": data}


def _schedule(proxy, args):
    d = _head("historical_schedule", args)
    # A session of the client: (reference date, start, end); of the official library: an object.
    d["historicalSessions"] = [_named("HistoricalSession", startDateTime=_part(s, 1, "startDateTime"), endDateTime=_part(s, 2, "endDateTime"),
                                      refDate=_part(s, 0, "refDate")) for s in args[4] or ()]
    return d


# Plain callback -> what makes the fields of its protobuf message (None: no message of
# its own, it is part of another callback's message).
_BUILDERS = {
    "error": _error,
    "tick_price": _tick_price,
    "tick_size": _tick_size,
    "tick_req_params": _tick_req_params,
    "open_order": _open_order,
    "completed_order": _completed_order,
    "update_portfolio": _with_contract("update_portfolio", 0),
    "position": _with_contract("position", 1),
    "position_multi": _with_contract("position_multi", 3),
    "contract_details": _contract_details(False),
    "bond_contract_details": _contract_details(True),
    "exec_details": _exec_details,
    "commission_and_fees_report": _commission_report,
    "historical_data": None,
    "historical_data_update": _historical_data_update,
    "scanner_data": None,
    "scanner_data_end": None,
    "security_definition_option_parameter": _sec_def_opt_parameter,
    "soft_dollar_tiers": _soft_dollar_tiers,
    "family_codes": _family_codes,
    "symbol_samples": _symbol_samples,
    "smart_components": _smart_components,
    "mkt_depth_exchanges": _depth_exchanges,
    "news_providers": _news_providers,
    "histogram_data": _histogram,
    "market_rule": _market_rule,
    "historical_ticks": _ticks("historicalTicks", _tick_fields),
    "historical_ticks_bid_ask": _ticks("historicalTicksBidAsk", _tick_bid_ask_fields),
    "historical_ticks_last": _ticks("historicalTicksLast", _tick_last_fields),
    "tick_by_tick_all_last": _tick_by_tick_last,
    "tick_by_tick_bid_ask": _tick_by_tick_bid_ask,
    "tick_by_tick_mid_point": _tick_by_tick_mid,
    "update_mkt_depth": _depth,
    "update_mkt_depth_l2": _depth_l2,
    "historical_schedule": _schedule,
}

_CLIENT_ERRORS = frozenset()


def _regular(name):
    def build(proxy, args):
        return _head(name, args)
    return build


def _nothing(self, _proto):
    pass


def defined(wrapper):
    """The protobuf callbacks `wrapper` defines itself."""
    out = set()
    for name in _PROTO_CALLBACKS:
        method = getattr(wrapper, name, None)
        if method is not None and getattr(method, "__func__", method) is not getattr(X.EWrapper, name, None):
            out.add(name)
    return out


class Callbacks:
    """Stands between the client and a wrapper that defines protobuf callbacks: a plain
    callback is preceded by the protobuf callback of its answer."""

    def __init__(self, wrapper, wanted):
        self._wrapper = wrapper
        self._wanted = wanted
        self._size = None           # the size of the price message being given
        self._size_given = None     # the (request, price tick) whose size was given with its price
        self._precisions = None     # the precisions of the request parameters being given

    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)
        wrapper = self._wrapper
        spec = T.ANSWERS.get(name)
        build = _BUILDERS.get(name, _regular(name)) if spec is not None and spec[0] in self._wanted else None
        if build is None:
            return getattr(wrapper, name)
        proto_callback, message = spec[0], spec[1]

        def call(*args):
            fields = build(self, args)
            if fields is not None:
                getattr(wrapper, proto_callback)(make(message, fields))
            return getattr(wrapper, name)(*args)
        self.__dict__[name] = call
        return call

    def price_has_size(self, size):
        """The next price callback is of a message that carries this size."""
        self._size = size

    def params_have_precisions(self, price, size):
        """The next request parameters are of a contract with these last price and last
        size precisions, which their message carries and the plain callback does not."""
        self._precisions = (price, size)

    def historical_data_of(self, req_id, bars):
        """The message of all the bars of an answer, before the callback of each bar."""
        if "historical_data_proto_buf" in self._wanted:
            fields = {"reqId": req_id, "historicalDataBars": [_bar_fields(b) for b in bars]}
            self._wrapper.historical_data_proto_buf(make("HistoricalData", fields))

    def scanner_data_of(self, req_id, rows):
        """The message of all the rows of a scan, before the callback of each row: each
        row (rank, contract details, distance, benchmark, projection, legs)."""
        if "scanner_data_proto_buf" in self._wanted:
            elements = []
            for rank, details, distance, benchmark, projection, combo_key in rows:
                e = _named("ScannerDataElement", rank=rank, marketName=details.marketName, distance=distance, benchmark=benchmark,
                           projection=projection, comboKey=combo_key)
                e["contract"] = _contract_fields(details.contract)
                elements.append(e)
            self._wrapper.scanner_data_proto_buf(make("ScannerData", {"reqId": req_id, "scannerDataElement": elements}))


def callbacks(wrapper):
    """What the client calls for `wrapper`: the wrapper itself, or a `Callbacks` over it
    when it defines protobuf callbacks."""
    wanted = defined(wrapper)
    return Callbacks(wrapper, wanted) if wanted else wrapper


_PROTO_CALLBACKS = ()
OFFICIAL_NAMES = {}     # the official names of the protobuf requests and callbacks of the client


def install(ibx, table):
    """Gives the client its protobuf requests and the wrapper its protobuf callbacks."""
    global T, X, _CLIENT_ERRORS, _PROTO_CALLBACKS
    T, X = table, ibx
    KINDS.update({message: dict(fields) for message, fields in table.MESSAGES.items()})
    _CLIENT_ERRORS = frozenset(table.CLIENT_ERRORS)
    for _id, (name, message, _level, argument) in table.REQUESTS.items():
        short = name.removesuffix("_proto_buf")
        if short in _REQUESTS:
            setattr(ibx.EClient, name, _request(short, message, _REQUESTS[short], argument))
    ibx.EClient.use_proto_buf = _use_proto_buf
    ibx.EClient.send_msg_proto_buf = _send_msg_proto_buf
    # The protobuf callback of each answer the client gives a plain callback for.
    names = sorted({spec[0] for plain, spec in table.ANSWERS.items() if hasattr(ibx.EWrapper, plain)})
    for name in names:
        if not hasattr(ibx.EWrapper, name):
            setattr(ibx.EWrapper, name, _nothing)
    _PROTO_CALLBACKS = tuple(names)
    OFFICIAL_NAMES.update({n: o for n, o in table.OFFICIAL_NAMES.items() if hasattr(ibx.EClient, n) or hasattr(ibx.EWrapper, n)})
