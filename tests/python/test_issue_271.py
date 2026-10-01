"""Issue #271: a call that waits for the engine (full command channel,
registration reply, engine stop) waits with the interpreter lock released,
so other Python threads keep running.

Each test starts the waiting call on a worker thread, with the fake engine
answering only after SERVE_DELAY_MS. The main thread then runs Python code:
it can only do so while the worker waits if the worker released the lock.
"""
import threading
import time

from ibx import Contract, EClient, EWrapper, Order

SERVE_DELAY_MS = 400


def runs_while_waiting(call):
    started = threading.Event()
    done = threading.Event()
    errors = []

    def worker():
        started.set()
        try:
            call()
        except BaseException as e:  # noqa: BLE001 - reported below
            errors.append(e)
        finally:
            done.set()

    t = threading.Thread(target=worker, daemon=True)
    t.start()
    assert started.wait(5)
    time.sleep(0.05)
    # Runs only when the worker does not hold the interpreter lock.
    ran_while_waiting = not done.is_set()
    assert done.wait(5), "the call never returned"
    t.join(5)
    assert errors == []
    return ran_while_waiting


def stock(con_id):
    c = Contract()
    c.con_id = con_id
    c.symbol = "TEST"
    c.sec_type = "STK"
    c.exchange = "SMART"
    c.currency = "USD"
    return c


def test_send_on_full_channel_releases_the_lock():
    c = EClient(EWrapper())
    c._test_connect("TEST123", control_capacity=1)
    c.req_ping()  # the channel is now full
    c._test_serve_commands_after(SERVE_DELAY_MS)
    assert runs_while_waiting(c.req_ping)


def test_registration_wait_releases_the_lock():
    c = EClient(EWrapper())
    c._test_connect("TEST123")
    c._test_serve_commands_after(SERVE_DELAY_MS)
    assert runs_while_waiting(lambda: c.req_tick_by_tick_data(1, stock(1001), "Last", 0, False))


def test_order_on_new_contract_waits_with_the_lock_released():
    c = EClient(EWrapper())
    c._test_connect("TEST123")
    c._test_serve_commands_after(SERVE_DELAY_MS)
    o = Order()
    o.action = "BUY"
    o.total_quantity = 1
    o.order_type = "LMT"
    o.lmt_price = 10.0
    assert runs_while_waiting(lambda: c.place_order(1, stock(1002), o))


def test_disconnect_on_full_channel_releases_the_lock():
    c = EClient(EWrapper())
    c._test_connect("TEST123", control_capacity=1)
    c.req_ping()  # the channel is now full: the stop command waits
    c._test_serve_commands_after(SERVE_DELAY_MS)
    assert runs_while_waiting(c.disconnect)
    assert not c.is_connected()
