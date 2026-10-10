"""ibx#565: on a paced session a request the Python client answers itself
returns at once, and the event loop answers it when its turn has come, in
the order the requests were made (ibx#563 for the test itself: the turns
are granted by the test, which plays the engine)."""

from ibx import EClient, EWrapper


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.events = []

    def current_time(self, time):
        self.events.append("current_time")

    def current_time_in_millis(self, time_in_millis):
        self.events.append("current_time_in_millis")

    def next_valid_id(self, order_id):
        self.events.append("next_valid_id")

    def managed_accounts(self, accounts_list):
        self.events.append("managed_accounts")

    def news_providers(self, news_providers):
        self.events.append("news_providers")

    def family_codes(self, family_codes):
        self.events.append("family_codes")

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.events.append(f"error {req_id} {error_code}")


def paced():
    w = Recorder()
    c = EClient(w)
    c._test_connect("TEST123")
    c._test_apply_logon(None, "APIELOG,SECDEFTA", 199, None, None)
    c._test_dispatch_once()
    w.events.clear()
    c._test_pace()
    return c, w


def test_the_call_returns_and_the_answer_comes_at_its_turn():
    c, w = paced()
    c.req_current_time()
    assert w.events == [], "put off: no answer inside the call"
    c._test_dispatch_once()
    assert w.events == [], "its turn has not come"
    assert c._test_grant_turns() == 1
    assert w.events == [], "the event loop answers, not the engine"
    c._test_dispatch_once()
    assert w.events == ["current_time"]


def test_the_answers_come_in_the_order_of_the_requests():
    c, w = paced()
    c.req_news_providers()
    c.req_ids(1)
    c.cancel_contract_data(7001)
    c.req_managed_accts()
    c.req_family_codes()
    c.req_current_time_in_millis()
    assert w.events == []
    # One turn after the other, as the pacing grants them.
    seen = []
    for _ in range(6):
        assert c._test_grant_turns(1) == 1
        c._test_dispatch_once()
        seen.append(list(w.events))
    assert [len(s) for s in seen] == [1, 2, 3, 4, 5, 6], seen
    assert w.events == ["news_providers", "next_valid_id", "error 7001 503", "managed_accounts", "family_codes",
                        "current_time_in_millis"]


def test_a_later_turn_does_not_overtake_an_earlier_request():
    c, w = paced()
    c.req_news_providers()
    c.req_family_codes()
    # Both turns granted at once: still in the order of the requests.
    assert c._test_grant_turns() == 2
    c._test_dispatch_once()
    assert w.events == ["news_providers", "family_codes"]


def test_without_pacing_the_answer_is_inside_the_call():
    w = Recorder()
    c = EClient(w)
    c._test_connect("TEST123")
    c._test_apply_logon(None, "APIELOG,SECDEFTA", 199, None, None)
    c._test_dispatch_once()
    w.events.clear()
    c.req_current_time()
    assert w.events == ["current_time"]


def test_disconnect_drops_the_requests_still_waiting():
    c, w = paced()
    c.req_news_providers()
    c.disconnect()
    assert "news_providers" not in w.events
