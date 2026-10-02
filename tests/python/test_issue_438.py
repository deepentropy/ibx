"""ibx#438: a continuous futures row has secType CONTFUT, and a bond row is
a bond contract details message, as the reference sends them."""

from ibx import EClient, EWrapper


class Recorder(EWrapper):
    def __init__(self):
        super().__init__()
        self.events = []

    def contract_details(self, req_id, contract_details):
        self.events.append(("contract_details", req_id, contract_details.contract.sec_type))

    def bond_contract_details(self, req_id, contract_details):
        self.events.append(("bond_contract_details", req_id, contract_details.contract.sec_type))

    def contract_details_end(self, req_id):
        self.events.append(("contract_details_end", req_id))


def connected():
    w = Recorder()
    c = EClient(w)
    c._test_connect("TEST123")
    return c, w


def test_continuous_future_row_is_contfut():
    c, w = connected()
    c._test_push_contract_row(9480, 515416632, "FUT", True)
    c._test_dispatch_once()
    assert w.events == [("contract_details", 9480, "CONTFUT"), ("contract_details_end", 9480)]


def test_bond_row_is_bond_contract_details():
    c, w = connected()
    c._test_push_contract_row(9488, 29105555, "BOND")
    c._test_dispatch_once()
    assert w.events == [("bond_contract_details", 9488, "BOND"), ("contract_details_end", 9488)]
