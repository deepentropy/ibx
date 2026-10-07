"""ibx#424: the display group requests are answered locally, as the reference
(capture of 07/10/2026): the fixed list of groups, `none` at once for a
subscription, error 321 with id -1 for a refusal, nothing for a valid update."""

from ibx import EClient, EWrapper


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.events = []

    def display_group_list(self, req_id, groups):
        self.events.append(("list", req_id, groups))

    def display_group_updated(self, req_id, contract_info):
        self.events.append(("updated", req_id, contract_info))

    def error(self, req_id, error_code, error_string, advanced_order_reject_json=""):
        self.events.append(("error", req_id, error_code, error_string))


def connected():
    w = Recorder()
    c = EClient(w)
    c._test_connect("TEST123")
    c._test_serve_commands_after(0)
    return c, w


def refusal(cls, cause):
    return ("error", -1, 321, f"Error validating request.-'{cls}' : cause - {cause}")


def test_query_gives_the_seven_groups():
    c, w = connected()
    c.query_display_groups(1)
    assert w.events == [("list", 1, "1|2|3|4|5|6|7")]


def test_subscribe_answers_none_at_once_and_refuses_bad_input():
    c, w = connected()
    c.subscribe_to_group_events(2, 1)
    for group in (9, 0, 8, -1):
        c.subscribe_to_group_events(3, group)
    c.subscribe_to_group_events(2, 1)
    assert w.events == [
        ("updated", 2, "none"),
        refusal("bX", "Invalid window group ID=9"),
        refusal("bX", "Invalid window group ID=0"),
        refusal("bX", "Invalid window group ID=8"),
        refusal("bX", "Invalid window group ID=-1"),
        refusal("bX", "Request with ID=2 was already subscribed."),
    ]


def test_update_needs_a_subscription_and_a_contract():
    c, w = connected()
    c.update_display_group(8, "265598@SMART")
    c.subscribe_to_group_events(2, 1)
    w.events.clear()
    c.update_display_group(2, "none")
    c.update_display_group(2, "abc@SMART")
    c.update_display_group(2, "0@SMART")
    c.update_display_group(2, "265598@SMART|foo=1")
    c.update_display_group(2, "265598@SMART|action=Foo")
    assert w.events == [
        refusal("bZ", "Request with ID=2 failed with invalid contract info=abc@SMART, expected format 'contractId@exchange'"),
        refusal("bZ", "Request with ID=2 failed with invalid contract info=0@SMART: conid or excahge are missing, "
                      "expected format 'contractId@exchange'"),
        refusal("bZ", "Action is unknown. Please check the pattern: conid@exch|param1=value1|...|action=(action)"),
        refusal("bZ", "Action 'Foo' is unknown"),
    ]
    w.events.clear()
    # A valid update gives nothing at once.
    c.update_display_group(2, "265598@SMART")
    assert w.events == []


def test_update_without_subscription_is_refused():
    c, w = connected()
    c.update_display_group(8, "265598@SMART")
    assert w.events == [refusal("bZ", "Request with ID=8 failed since request ID wasn't found.")]


def test_unsubscribe_of_an_unknown_request_id_is_refused():
    c, w = connected()
    c.unsubscribe_from_group_events(9)
    c.subscribe_to_group_events(2, 1)
    c.unsubscribe_from_group_events(2)
    c.unsubscribe_from_group_events(2)
    assert w.events == [
        refusal("bY", "Subscription for Group Events with request ID=9 wasn't found."),
        ("updated", 2, "none"),
        refusal("bY", "Subscription for Group Events with request ID=2 wasn't found."),
    ]
