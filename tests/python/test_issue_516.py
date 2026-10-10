"""ibx#516: the seven requests of the official client library added to ibx
give, on a connected client, the answers of the reference run of 10/10/2026;
on a client that is not connected, 504 with the id of the request."""

import time

from ibx import EClient, EWrapper

CONTRACT_DATA = "The TWS is out of date and must be upgraded.  It does not support contract data cancels."
HISTORICAL_TICKS = "The TWS is out of date and must be upgraded.  It does not support historical ticks cancels."
INTENT = "  Intent to authenticate needs to be expressed during initial connect request."
VERIFY_REQUEST = "Verify Request Sending Error - " + INTENT
VERIFY_AND_AUTH_REQUEST = "Verify And Auth Request Sending Error - " + INTENT
VERIFY_MESSAGE = ("ApiVerify error:ApiVerifyMessage ignored. Message sequence error. "
                  "State: verifyStatus=NONE, verifyInProgress=false")


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.events = []

    def current_time_in_millis(self, time_in_millis):
        self.events.append(("millis", time_in_millis))

    def verify_message_api(self, api_data):
        self.events.append(("verify_message_api", api_data))

    def verify_completed(self, is_successful, error_text):
        self.events.append(("verify_completed", is_successful, error_text))

    def verify_and_auth_message_api(self, api_data, xyz_challange):
        self.events.append(("verify_and_auth_message_api", api_data, xyz_challange))

    def verify_and_auth_completed(self, is_successful, error_text):
        self.events.append(("verify_and_auth_completed", is_successful, error_text))

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.events.append(("error", req_id, error_code, error_string))


def connected(offset=None):
    w = Recorder()
    c = EClient(w)
    c._test_connect("TEST123")
    c._test_apply_logon(offset, "APIELOG,SECDEFTA", 199, None, None)
    return c, w


def test_current_time_in_millis_adds_the_logon_offset():
    c, w = connected(offset=60_000)
    c.req_current_time_in_millis()
    (kind, t), = w.events
    assert kind == "millis" and isinstance(t, int)
    assert abs(t - int(time.time() * 1000) - 60_000) <= 1_000


def test_the_two_cancels_are_refused_for_their_request():
    c, w = connected()
    c.cancel_contract_data(7001)
    c.cancel_historical_ticks(7003)
    c._test_dispatch_once()
    assert w.events == [("error", 7001, 503, CONTRACT_DATA), ("error", 7003, 503, HISTORICAL_TICKS)]


def test_the_verify_requests_answer_as_the_reference():
    c, w = connected()
    c.verify_request("app", "1.0")
    c.verify_message("data")
    c.verify_and_auth_request("app", "1.0", "key")
    c.verify_and_auth_message("data", "response")
    c._test_dispatch_once()
    assert w.events == [
        ("error", -1, 544, VERIFY_REQUEST),
        ("error", -1, 10095, VERIFY_MESSAGE),
        ("error", -1, 551, VERIFY_AND_AUTH_REQUEST),
    ]


def test_not_connected_gives_504():
    w = Recorder()
    c = EClient(w)
    c.req_current_time_in_millis()
    c.cancel_contract_data(7001)
    c.cancel_historical_ticks(7003)
    c.verify_request("app", "1.0")
    c.verify_message("data")
    c.verify_and_auth_request("app", "1.0", "key")
    c.verify_and_auth_message("data", "response")
    assert w.events == [("error", i, 504, "Not connected") for i in (-1, 7001, 7003, -1, -1, -1, -1)]


def test_the_verify_callbacks_can_be_called():
    w = Recorder()
    w.verify_message_api("d")
    w.verify_completed(True, "")
    w.verify_and_auth_message_api("d", "c")
    w.verify_and_auth_completed(False, "no")
    assert [e[0] for e in w.events] == ["verify_message_api", "verify_completed",
                                        "verify_and_auth_message_api", "verify_and_auth_completed"]
    base = EWrapper()
    base.current_time_in_millis(1)
    base.verify_message_api("d")
    base.verify_completed(True, "")
    base.verify_and_auth_message_api("d", "c")
    base.verify_and_auth_completed(False, "no")
