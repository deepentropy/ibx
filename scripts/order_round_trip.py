"""An order's life on the paper account: placed, changed, withdrawn.

The paper suite's order phases name US shares, so they wait for the New York
session and skip outside it — which is most of the hours anyone works in. This
asks the same question of a contract that trades nearly around the clock, so
the order path can be checked at three in the morning as well as at noon.

A limit far below the market, so it rests and never trades. What is checked is
the round trip: that the venue takes each step, and that this client's record
of the order moves with it. Nothing is bought.

Paper only, and it says so on the connection rather than trusting a default.

    IB_USERNAME=… IB_PASSWORD=… python scripts/order_round_trip.py
"""

import os
import sys
import time

import ibx
from ibx import Contract, Order

#: The front month of the smaller S&P future, which trades nearly around the
#: clock. Rolled by hand: a contract that has expired is refused by name, which
#: is a clear failure rather than a quiet one.
#:
#: A share can be named instead, which is worth doing during a session or in the
#: hours either side of one — the order path is the same and the venues are not:
#:
#:     IBX_RT_SYMBOL=SPY IBX_RT_SEC_TYPE=STK IBX_RT_EXCHANGE=SMART \
#:     IBX_RT_PRICE=400 IBX_RT_OUTSIDE_RTH=1 python scripts/order_round_trip.py
SYMBOL = os.environ.get("IBX_RT_SYMBOL", "MES")
SEC_TYPE = os.environ.get("IBX_RT_SEC_TYPE", "FUT")
EXCHANGE = os.environ.get("IBX_RT_EXCHANGE", "CME")
EXPIRY = os.environ.get("IBX_RT_EXPIRY", "202612")

#: Far enough under the market that it cannot trade whatever the market is
#: doing, and on the contract's own increment. Stated rather than read off a
#: quote: this check is about the order path, and asking for a quote makes it
#: need an entitlement it does not otherwise use.
RESTS_AT = float(os.environ.get("IBX_RT_PRICE", "6000"))

#: Whether the order may work outside the regular session. A share resting
#: before the bell needs this said; a future does not have the distinction.
OUTSIDE_RTH = os.environ.get("IBX_RT_OUTSIDE_RTH", "") not in ("", "0")

#: How long the venue is given to answer each step.
ANSWER = 20


def settle(trade, what, wanted):
    """Wait for the order to reach one of `wanted`, and say what happened."""
    deadline = time.time() + ANSWER
    while time.time() < deadline:
        status = trade.orderStatus.status
        if status in wanted:
            print(f"{what}: {status}", flush=True)
            return status
        time.sleep(0.2)
    print(f"{what}: nothing within {ANSWER}s, still {trade.orderStatus.status}", flush=True)
    return None


def main() -> int:
    username = os.environ.get("IB_USERNAME", "")
    password = os.environ.get("IB_PASSWORD", "")
    if not username.strip() or not password.strip():
        print("IB_USERNAME/IB_PASSWORD unset. This places an order on the paper account.")
        return 2

    ib = ibx.IB()
    ib.connect(
        os.environ.get("IB_HOST", "cdc1.ibllc.com"), 0, clientId=1,
        username=username, password=password, paper=True,
    )

    contract = Contract()
    contract.symbol, contract.secType, contract.exchange = SYMBOL, SEC_TYPE, EXCHANGE
    contract.currency = "USD"
    if SEC_TYPE == "FUT":
        contract.lastTradeDateOrContractMonth = EXPIRY
    (contract,) = ib.qualifyContracts(contract)
    print(f"contract: conId={contract.conId} {contract.localSymbol}", flush=True)

    order = Order()
    order.action, order.orderType, order.totalQuantity = "BUY", "LMT", 1
    order.lmtPrice, order.tif = RESTS_AT, "GTC"
    order.outsideRth = OUTSIDE_RTH
    trade = ib.placeOrder(contract, order)

    placed = settle(trade, "placed   ", {"Submitted", "PreSubmitted"})
    if placed is None:
        ib.disconnect()
        return 1
    print(f"  resting at {trade.order.lmtPrice}", flush=True)

    # The same order at a new price, which is what a replace is.
    moved_to = RESTS_AT - 1.0
    order.lmtPrice = moved_to
    ib.placeOrder(contract, order)
    time.sleep(5)
    held = trade.order.lmtPrice
    print(f"changed  : this client holds {held}, asked for {moved_to}", flush=True)

    ib.cancelOrder(order)
    withdrawn = settle(trade, "withdrawn", {"Cancelled", "ApiCancelled"})

    # Nothing of it is left working at the venue, which is the half a status
    # does not answer.
    left = [t.order.orderId for t in ib.openTrades()]
    print(f"left working: {left}", flush=True)
    ib.disconnect()

    whole = bool(placed) and bool(withdrawn) and held == moved_to and order.orderId not in left
    print("the round trip is whole" if whole else "the round trip is not whole", flush=True)
    return 0 if whole else 1


if __name__ == "__main__":
    sys.exit(main())
